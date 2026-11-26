//! An operator drain end to end (#49 D3): one gateway on PostgreSQL as the
//! runtime role, the real HTTP router, the OpenAI adapter and a mock
//! upstream. A drained route takes no new attempt, even for a request planned
//! from health cached before the drain was set, and the drain differs from
//! disabling the route: the model stays listed and is temporarily
//! unavailable, never missing.
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;

use super::{
    Engine, EngineLimits,
    admission_validation_tests::{add_deployment, issue_key, serve},
};
use crate::{
    governance::tests::db::fixture,
    providers::{
        ProviderRegistry,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
    store::Store,
};

struct Resolves;
impl SecretResolver for Resolves {
    fn resolve(&self, _: &str) -> Result<Secret, super::error::InferenceError> {
        Secret::new("test-provider-key".into())
    }
}

/// A mock upstream answering every chat completion at once.
fn upstream(hits: Arc<AtomicUsize>) -> Router {
    Router::new().fallback(move || {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"id":"c1","object":"chat.completion","created":1,"model":"m",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}))
        }
    })
}

#[dbtest::test]
async fn a_drained_route_takes_no_new_attempt_and_stays_listed(pool: PgPool) {
    let mut f = fixture(pool.clone()).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, _mock) = serve(upstream(hits.clone())).await;
    let deployment = add_deployment(&f, "openai", "env:OMG_DRAIN_TEST_KEY", None, "m", 0).await;
    sqlx::raw_sql(include_str!("../../tests/common/ensure_runtime_role.sql"))
        .execute(&pool)
        .await
        .unwrap();
    f.store.apply_runtime_grants().await.unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let store = Store::new(runtime);
    // As `serve` runs it: per-replica caches on.
    store.caches.versions.enable();
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolves),
            format!("{mock_base}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let (base, _server) = serve(crate::http::router_with_engine(store.clone(), None, engine)).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let ask = || {
        let request = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&token)
            .json(&json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}));
        async move {
            let response = request.send().await.unwrap();
            let retry: Option<u64> = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse().ok());
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            (
                status,
                retry,
                body["error"]["code"].as_str().map(str::to_owned),
                body,
            )
        }
    };
    let listed = || async {
        let v: serde_json::Value = http
            .get(format!("{base}/v1/models"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "company/smart")
    };
    // As the management path writes it (its own tests cover the API).
    let set = |until: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!("UPDATE deployments SET drain_reason='Replacing the GPU node', drain_until={until}, drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1"))
                .bind(deployment)
                .execute(&pool)
                .await
                .unwrap();
        }
    };

    // Serving, and its health now cached by this replica.
    assert_eq!(ask().await.0, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Drained while this replica still holds health read before the drain
    // (as another replica does for up to `HEALTH_TTL`): the plan comes from
    // that entry and still names the route, but admission refuses it under
    // the catalog lock. Nothing is sent.
    store.caches.store_health(vec![(
        deployment,
        crate::cache::HealthEntry {
            // Fresh for as long as the test needs it.
            loaded: std::time::Instant::now() + Duration::from_secs(3600),
            open_until: None,
            consecutive_failures: 0,
            observed_age: Some(Duration::ZERO),
            probe_until: None,
            reopen_count: 0,
            drained: false,
            drain_until: None,
        },
    )]);
    set("NULL").await;
    let (status, retry, code, body) = ask().await;
    assert_eq!(
        (status, retry, code.as_deref()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            None,
            Some("model_temporarily_unavailable")
        )
    );
    assert!(
        !body.to_string().contains("GPU"),
        "the operator's reason stays internal"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Once the cache is read again, planning itself leaves the route out.
    store.caches.forget_health(deployment);
    assert_eq!(ask().await.0, StatusCode::SERVICE_UNAVAILABLE);
    // A drain that ends soon: retry after it.
    set("clock_timestamp()+interval '120 seconds'").await;
    store.caches.forget_health(deployment);
    let (status, retry, _, _) = ask().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(retry.is_some_and(|s| (100..=120).contains(&s)), "{retry:?}");
    // The difference from disabling: drained, the model is still listed and
    // only temporarily unavailable.
    assert!(listed().await);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(deployment)
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    store.caches.forget_health(deployment);
    assert!(!listed().await, "disabled, the model is no longer listed");
    let (status, _, code, _) = ask().await;
    assert_eq!(
        (status, code.as_deref()),
        (StatusCode::NOT_FOUND, Some("model_not_found"))
    );
    // Enabled again with the drain ended (its `until` passed): serving.
    sqlx::query("UPDATE deployments SET enabled=true, drain_until=clock_timestamp()-interval '1 millisecond' WHERE id=$1")
        .bind(deployment)
        .execute(&pool)
        .await
        .unwrap();
    store.caches.forget_health(deployment);
    let mut served = false;
    for _ in 0..50 {
        // The catalog change reaches this replica's route snapshot by its
        // change notification.
        if ask().await.0 == StatusCode::OK {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(served);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// Two routes; the first drained after this replica read its health (as
/// another replica's planning may still see it for up to `HEALTH_TTL`). The
/// plan names the first, admission refuses it, and the second serves the
/// request as its one attempt: nothing reaches the drained route.
#[dbtest::test]
async fn a_route_drained_after_the_plan_is_passed_over_and_the_next_serves(pool: PgPool) {
    let mut f = fixture(pool.clone()).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, _mock) = serve(upstream(hits.clone())).await;
    let first = add_deployment(&f, "openai", "env:OMG_DRAIN_TEST_KEY", None, "first", 0).await;
    let second = add_deployment(&f, "openai", "env:OMG_DRAIN_TEST_KEY", None, "second", 1).await;
    for (deployment, priority) in [(first, 0), (second, 1)] {
        sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,$2)")
            .bind(deployment)
            .bind(priority)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    let store = Store::new(f.store.pool.clone());
    store.caches.versions.enable();
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolves),
            format!("{mock_base}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let (base, _server) = serve(crate::http::router_with_engine(store.clone(), None, engine)).await;
    // Health of both read before the drain, still fresh on this replica.
    let fresh = |id| {
        (
            id,
            crate::cache::HealthEntry {
                loaded: std::time::Instant::now() + Duration::from_secs(3600),
                open_until: None,
                consecutive_failures: 0,
                observed_age: Some(Duration::ZERO),
                probe_until: None,
                reopen_count: 0,
                drained: false,
                drain_until: None,
            },
        )
    };
    store.caches.store_health(vec![fresh(first), fresh(second)]);
    sqlx::query("UPDATE deployments SET drain_reason='maintenance', drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&token)
        .json(&json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let attempts: Vec<(uuid::Uuid, i32)> = sqlx::query_as(
        "SELECT deployment_id, attempt_number FROM inference_executions WHERE deployment_id = ANY($1)",
    )
    .bind(vec![first, second])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        attempts,
        vec![(second, 1)],
        "one attempt, on the second route"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one upstream call");
}
