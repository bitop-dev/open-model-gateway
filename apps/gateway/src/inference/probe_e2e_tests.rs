//! Half-open recovery end to end (#49): two gateways (replicas with their
//! own caches) on PostgreSQL as the runtime role, the real HTTP router, the
//! OpenAI adapter and a mock upstream. After a cooldown exactly one request
//! reaches the upstream; the others are told the route is cooling down;
//! the probe's outcome reopens (for a doubled cooldown) or closes the circuit.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, http::StatusCode, response::IntoResponse};
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

const FAIL: u8 = 0;
const HELD_OK: u8 = 1;

/// A mock upstream: 503 (a counted failure), or a chat completion held until
/// the test releases it (`entered` is signalled when it arrives).
fn upstream(
    hits: Arc<AtomicUsize>,
    mode: Arc<AtomicU8>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> Router {
    Router::new().fallback(move || {
        let (hits, mode, entered, release) =
            (hits.clone(), mode.clone(), entered.clone(), release.clone());
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            if mode.load(Ordering::SeqCst) == FAIL {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error":{"type":"server_error","message":"down"}})),
                )
                    .into_response();
            }
            entered.notify_one();
            release.notified().await;
            axum::Json(json!({"id":"c1","object":"chat.completion","created":1,"model":"m",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}))
            .into_response()
        }
    })
}

/// Status, `Retry-After` seconds and error code of one answer.
type Answer = (StatusCode, Option<u64>, Option<String>);

#[dbtest::test]
async fn after_a_cooldown_one_request_across_replicas_decides(pool: PgPool) {
    let mut f = fixture(pool.clone()).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (hits, mode) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU8::new(FAIL)));
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let (mock_base, _mock) = serve(upstream(
        hits.clone(),
        mode.clone(),
        entered.clone(),
        release.clone(),
    ))
    .await;
    let deployment = add_deployment(&f, "openai", "env:OMG_PROBE_TEST_KEY", None, "m", 0).await;
    // Threshold 1, a cooldown of 60 s that the test ends by writing the row.
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,60)")
        .bind(deployment).execute(&f.store.pool).await.unwrap();
    // The runtime role, with its grants (the probe columns included).
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
    let (mut gateways, mut stores, mut servers) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..2 {
        let store = Store::new(runtime.clone());
        let mut registry = ProviderRegistry::default();
        registry
            .register(Arc::new(OpenAiAdapter::for_test(
                Arc::new(Resolves),
                format!("{mock_base}/v1"),
            )))
            .unwrap();
        let engine =
            Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
        let (base, server) =
            serve(crate::http::router_with_engine(store.clone(), None, engine)).await;
        gateways.push(base);
        stores.push(store);
        servers.push(server);
    }
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let ask = |base: String| {
        let request = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&token)
            .json(&json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}));
        async move {
            let response = request.send().await.unwrap();
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse().ok());
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            let answer: Answer = (
                status,
                retry,
                body["error"]["code"].as_str().map(str::to_owned),
            );
            answer
        }
    };
    // (the cooldown the last opening set, from the same statement's clock;
    // reopen count; probe held).
    let circuit = || async {
        sqlx::query_as::<_, (Option<i64>, i32, bool)>("SELECT round(EXTRACT(EPOCH FROM open_until-last_observed_at))::int8, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1")
            .bind(deployment)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    // The cooldown passes now, for both gateways (their health caches too).
    let expire = || async {
        sqlx::query("UPDATE deployment_health SET open_until=clock_timestamp()-interval '1 millisecond' WHERE deployment_id=$1")
            .bind(deployment)
            .execute(&pool)
            .await
            .unwrap();
        for store in &stores {
            store.caches.forget_health(deployment);
        }
    };
    let cooling = |answer: &Answer, retry: std::ops::RangeInclusive<u64>| {
        answer.0 == StatusCode::SERVICE_UNAVAILABLE
            && answer.1.is_some_and(|s| retry.contains(&s))
            && answer.2.as_deref() == Some("model_temporarily_unavailable")
    };

    // A counted failure opens the circuit for 60 s.
    assert!(!ask(gateways[0].clone()).await.0.is_success());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(circuit().await, (Some(60), 1, false));
    // While it is open, neither gateway sends anything; each reports the
    // remaining cooldown.
    for base in &gateways {
        let answer = ask(base.clone()).await;
        assert!(cooling(&answer, 1..=60), "{answer:?}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Half-open: the probe fails, and the circuit reopens for twice the cooldown.
    expire().await;
    assert!(!ask(gateways[1].clone()).await.0.is_success());
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(circuit().await, (Some(120), 2, false));

    // Half-open again, the upstream recovered and holds its answer: six
    // requests on both gateways; one is the probe, and the five others are
    // answered while it is held, told to come back within 5 s.
    expire().await;
    mode.store(HELD_OK, Ordering::SeqCst);
    let mut asked = tokio::task::JoinSet::new();
    for i in 0..6 {
        asked.spawn(ask(gateways[i % 2].clone()));
    }
    tokio::time::timeout(Duration::from_secs(60), entered.notified())
        .await
        .expect("the probe reaches the upstream");
    // The five others are answered while the probe is still held.
    let mut answers = Vec::new();
    while answers.len() < 5 {
        let answer = tokio::time::timeout(Duration::from_secs(60), asked.join_next())
            .await
            .expect("answered while the probe is held")
            .unwrap()
            .unwrap();
        answers.push(answer);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3, "{answers:?}");
    let soon = 1..=crate::routing::PROBE_RETRY_AFTER_SECONDS as u64;
    assert!(
        answers.iter().all(|a| cooling(a, soon.clone())),
        "{answers:?}"
    );
    release.notify_one();
    answers.push(
        tokio::time::timeout(Duration::from_secs(60), asked.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        answers.iter().filter(|a| a.0 == StatusCode::OK).count(),
        1,
        "{answers:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "exactly one request reached the upstream"
    );
    // The probe's success closed the circuit and forgot the openings: traffic
    // resumes (on the other gateway once its health cache, at most
    // `HEALTH_TTL` old, is read again; the test does not wait for that).
    assert_eq!(circuit().await, (None, 0, false));
    for store in &stores {
        store.caches.forget_health(deployment);
    }
    for base in &gateways {
        let answer = tokio::spawn(ask(base.clone()));
        tokio::time::timeout(Duration::from_secs(60), entered.notified())
            .await
            .expect("the request reaches the upstream");
        release.notify_one();
        assert_eq!(answer.await.unwrap().0, StatusCode::OK);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 5);
}

/// Every statement on the probe slot runs as the runtime role under its
/// column grants (`deploy/staging/runtime-grants.sql`): the claim, the
/// renewal of a slot claimed before a wait, the first-content close, the
/// release, and a decision fenced by the probe's epoch.
#[dbtest::test]
async fn the_runtime_role_can_run_every_probe_statement(pool: PgPool) {
    use crate::routing::{ProbeClaim, claim_probe, close_probe, release_probe, renew_probe};
    let f = fixture(pool.clone()).await;
    let deployment = add_deployment(&f, "openai", "env:OMG_PROBE_TEST_KEY", None, "m", 0).await;
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,60)")
        .bind(deployment).execute(&f.store.pool).await.unwrap();
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
    let hold = Duration::from_secs(60);
    let expire = || async {
        // As the owner: the test moves the clock, not the runtime.
        sqlx::query("UPDATE deployment_health SET open_until=clock_timestamp()-interval '1 millisecond' WHERE deployment_id=$1")
            .bind(deployment).execute(&f.store.pool).await.unwrap();
        store.caches.forget_health(deployment);
    };
    let claim = || async {
        match claim_probe(&store, deployment, uuid::Uuid::new_v4(), hold).await {
            Ok(ProbeClaim::Claimed(epoch)) => epoch,
            other => panic!("not claimed: {other:?}"),
        }
    };
    crate::routing::record_result(&store, deployment, Some(super::error::InferenceError::Busy))
        .await
        .expect("open the circuit");
    expire().await;
    let first = claim().await;
    assert!(
        renew_probe(&store, deployment, first, hold)
            .await
            .expect("renew")
    );
    release_probe(&store, deployment, first)
        .await
        .expect("release");
    let second = claim().await;
    crate::routing::record_probe_result(
        &store,
        deployment,
        Some(super::error::InferenceError::UpstreamUnavailable),
        second,
    )
    .await
    .expect("a failed probe reopens");
    expire().await;
    let third = claim().await;
    close_probe(&store, deployment, third).await.expect("close");
    crate::routing::record_probe_result(&store, deployment, None, third)
        .await
        .expect("a complete success");
    let (open, reopens, held): (bool, i32, bool) = sqlx::query_as(
        "SELECT open_until IS NOT NULL, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(deployment)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!((open, reopens, held), (false, 0, false));
}
