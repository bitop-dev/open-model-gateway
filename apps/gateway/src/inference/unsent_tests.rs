//! An admitted attempt that fails before its adapter built any upstream
//! request settles as a known zero; one that failed after the request was
//! sent keeps its hold as unknown, even with the same error class. The real
//! adapters, engine and HTTP routers on PostgreSQL, against a mock upstream
//! that counts what reaches it.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Router, http::StatusCode, response::IntoResponse};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::admission_validation_tests::{
    Server, add_deployment, add_unpriced_deployment, issue_key, serve, start_gateway,
};
use crate::{
    governance::tests::db::{Fixture, fixture},
    providers::{
        ProviderAdapter, ProviderRegistry,
        anthropic::AnthropicAdapter,
        bedrock::BedrockAdapter,
        local::LocalAdapter,
        local::{Profile, endpoints::ApprovedEndpoints},
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::{EnvSecrets, Secret, SecretResolver},
    },
};

/// Resolves every reference to a fixed test value.
struct Resolves;
impl SecretResolver for Resolves {
    fn resolve(&self, _: &str) -> Result<Secret, super::error::InferenceError> {
        Secret::new("test-provider-key".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Anthropic,
    /// The Messages client protocol on an Anthropic route (#812's report).
    AnthropicMessages,
    OpenAi,
    Responses,
    OpenRouter,
    Local,
    Bedrock,
}
impl Kind {
    fn provider(self) -> &'static str {
        match self {
            Self::Anthropic | Self::AnthropicMessages => "anthropic",
            Self::OpenAi | Self::Responses => "openai",
            Self::OpenRouter => "openrouter",
            Self::Local => "vllm",
            Self::Bedrock => "bedrock",
        }
    }
    /// The connection's credential reference: one the adapter's own
    /// credential path refuses when `resolvable` is false.
    fn credential(self, resolvable: bool) -> &'static str {
        match (self, resolvable) {
            (Self::Local, true) => "none",
            (Self::Bedrock, _) => "aws:default",
            _ => "env:OMG_UNSENT_TEST_KEY",
        }
    }
    fn region(self) -> Option<&'static str> {
        (self == Self::Bedrock).then_some("us-east-1")
    }
    /// The adapter against `upstream`. Unresolvable: the resolver refuses
    /// every reference (`EnvSecrets` with nothing allowed), or for Bedrock
    /// the SDK's credential provider fails (a named profile that does not
    /// exist), so the request never gets an identity.
    fn adapter(self, upstream: &str, resolvable: bool) -> Arc<dyn ProviderAdapter> {
        let resolver: Arc<dyn SecretResolver> = if resolvable {
            Arc::new(Resolves)
        } else {
            Arc::new(EnvSecrets::new(Vec::new()))
        };
        let base = format!("{upstream}/v1");
        match self {
            Self::Anthropic | Self::AnthropicMessages => {
                Arc::new(AnthropicAdapter::for_test(resolver, base))
            }
            Self::OpenAi | Self::Responses => Arc::new(OpenAiAdapter::for_test(resolver, base)),
            Self::OpenRouter => Arc::new(OpenRouterAdapter::for_test(
                resolver,
                OpenRouterConfig::default(),
                base,
            )),
            Self::Local => Arc::new(LocalAdapter::new(
                Profile::Vllm,
                resolver,
                Arc::new(ApprovedEndpoints::for_test(&local_endpoint(upstream))),
            )),
            Self::Bedrock if resolvable => Arc::new(BedrockAdapter::for_test(upstream)),
            Self::Bedrock => Arc::new(BedrockAdapter::for_test_without_credentials(upstream)),
        }
    }
    fn request(self, base: &str, token: &str, stream: bool) -> reqwest::RequestBuilder {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let messages = json!([{"role":"user","content":"hi"}]);
        match self {
            Self::AnthropicMessages => http
                .post(format!("{base}/v1/messages"))
                .header("anthropic-version", "2023-06-01")
                .json(&json!({"model":"company/smart","max_tokens":16,"stream":stream,"messages":messages})),
            Self::Responses => http
                .post(format!("{base}/v1/responses"))
                .json(&json!({"model":"company/smart","input":"hi","max_output_tokens":16,"stream":stream})),
            _ => http
                .post(format!("{base}/v1/chat/completions"))
                .json(&json!({"model":"company/smart","max_tokens":16,"stream":stream,"messages":messages})),
        }
        .bearer_auth(token)
    }
}
fn local_endpoint(upstream: &str) -> String {
    format!("{}/v1", upstream.replace("127.0.0.1", "fixture.invalid"))
}

/// A mock that counts requests and answers each with `status`.
fn counting(hits: Arc<AtomicUsize>, status: StatusCode) -> Router {
    Router::new().fallback(move || {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            (
                status,
                axum::Json(json!({"error":{"type":"test","message":"test"}})),
            )
                .into_response()
        }
    })
}

/// A gateway over one priced deployment of `kind`.
struct World {
    f: Fixture,
    token: String,
    base: String,
    hits: Arc<AtomicUsize>,
    _servers: Vec<Server>,
}
async fn world(pool: PgPool, kind: Kind, resolvable: bool, upstream: StatusCode) -> World {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, mock) = serve(counting(hits.clone(), upstream)).await;
    add_deployment(
        &f,
        kind.provider(),
        kind.credential(resolvable),
        kind.region(),
        "private-model",
        0,
    )
    .await;
    if kind == Kind::Local {
        sqlx::query("UPDATE provider_connections SET endpoint=$1 WHERE provider='vllm'")
            .bind(local_endpoint(&mock_base))
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    let mut registry = ProviderRegistry::default();
    registry
        .register(kind.adapter(&mock_base, resolvable))
        .unwrap();
    let (base, gateway) = start_gateway(&f, registry).await;
    World {
        f,
        token,
        base,
        hits,
        _servers: vec![mock, gateway],
    }
}

/// What one request left: (execution state, reservation state, held,
/// actual, input tokens, output tokens, ledger kinds and amounts).
type Recorded = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Vec<(String, Option<i64>)>,
);
async fn recorded(pool: &PgPool, request: Uuid) -> Vec<Recorded> {
    type Row = (
        Uuid,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<Row> =
        sqlx::query_as(
            "SELECT e.id,e.state,r.state,r.held_microusd,r.actual_microusd,r.input_tokens,r.output_tokens FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.root_request_id=$1 ORDER BY e.attempt_number",
        )
        .bind(request)
        .fetch_all(pool)
        .await
        .unwrap();
    let mut out = Vec::new();
    for (id, execution, reservation, held, actual, input, output) in rows {
        let ledger = sqlx::query_as(
            "SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap();
        out.push((execution, reservation, held, actual, input, output, ledger));
    }
    out
}
/// The workspace's lifetime totals: (held, unknown count, pending count).
async fn totals(f: &Fixture) -> (String, i64, i64) {
    sqlx::query_as("SELECT held_microusd::text,unknown,pending FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime'")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}
async fn call(w: &World, kind: Kind, stream: bool) -> (StatusCode, Uuid, String) {
    let response = kind
        .request(&w.base, &w.token, stream)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    (status, id, response.text().await.unwrap())
}

/// A credential the adapter's own path refuses: nothing reaches the
/// upstream, and the attempt settles at zero with its tokens zero, its
/// ledger settlement entry and nothing left held.
async fn nothing_sent(pool: PgPool, kind: Kind) {
    let w = world(pool, kind, false, StatusCode::OK).await;
    for stream in [false, true] {
        let (status, id, body) = call(&w, kind, stream).await;
        let label = format!("{kind:?} stream={stream}: {status} {body}");
        assert!(!status.is_success(), "{label}");
        assert_eq!(w.hits.load(Ordering::SeqCst), 0, "{label}");
        let rows = recorded(&w.f.store.pool, id).await;
        assert_eq!(rows.len(), 1, "{label}");
        let (execution, reservation, held, actual, input, output, ledger) = &rows[0];
        assert_eq!(execution, "failed", "{label}");
        assert!(held.is_some_and(|h| h > 0), "{label}: admitted with a hold");
        assert_eq!(
            (reservation.as_str(), *actual, *input, *output),
            ("settled", Some(0), Some(0), Some(0)),
            "{label}"
        );
        assert_eq!(
            ledger,
            &vec![("hold".into(), *held), ("settlement".into(), Some(0))],
            "{label}"
        );
        assert_eq!(totals(&w.f).await, ("0".into(), 0, 0), "{label}");
    }
}

/// The request reached the upstream: the hold stays, as unknown. A 401 has
/// the class (`Configuration`) a credential failure has; a 500 is an
/// ordinary upstream failure.
async fn sent(pool: PgPool, kind: Kind, upstream: StatusCode) {
    let w = world(pool, kind, true, upstream).await;
    let mut held_total = 0;
    for (n, stream) in [false, true].into_iter().enumerate() {
        let (status, id, body) = call(&w, kind, stream).await;
        let label = format!("{kind:?} stream={stream} upstream={upstream}: {status} {body}");
        assert!(!status.is_success(), "{label}");
        assert_eq!(w.hits.load(Ordering::SeqCst), n + 1, "{label}");
        let rows = recorded(&w.f.store.pool, id).await;
        assert_eq!(rows.len(), 1, "{label}");
        let (execution, reservation, held, actual, input, output, ledger) = &rows[0];
        assert_eq!(execution, "failed", "{label}");
        assert!(held.is_some_and(|h| h > 0), "{label}");
        assert_eq!(
            (reservation.as_str(), *actual, *input, *output),
            ("unknown", None, None, None),
            "{label}"
        );
        assert_eq!(
            ledger,
            // The unknown entry records the known floor: nothing was reported.
            &vec![("hold".into(), *held), ("unknown".into(), Some(0))],
            "{label}"
        );
        held_total += held.unwrap();
        assert_eq!(
            totals(&w.f).await,
            (held_total.to_string(), n as i64 + 1, 0),
            "{label}"
        );
    }
}

macro_rules! per_kind {
    ($($name:ident: $kind:expr,)*) => {$(
        mod $name {
            use super::*;
            #[dbtest::test]
            async fn a_credential_failure_before_anything_was_sent_settles_as_a_known_zero(pool: PgPool) {
                nothing_sent(pool, $kind).await;
            }
            #[dbtest::test]
            async fn an_upstream_401_after_the_request_was_sent_keeps_its_hold(pool: PgPool) {
                sent(pool, $kind, StatusCode::UNAUTHORIZED).await;
            }
            #[dbtest::test]
            async fn an_upstream_500_after_the_request_was_sent_keeps_its_hold(pool: PgPool) {
                sent(pool, $kind, StatusCode::INTERNAL_SERVER_ERROR).await;
            }
        }
    )*};
}
per_kind! {
    anthropic: Kind::Anthropic,
    anthropic_messages: Kind::AnthropicMessages,
    openai_chat: Kind::OpenAi,
    openai_responses: Kind::Responses,
    openrouter: Kind::OpenRouter,
    local: Kind::Local,
    bedrock: Kind::Bedrock,
}

#[dbtest::test]
async fn an_unpriced_route_with_nothing_sent_also_settles_as_a_known_zero(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    add_unpriced_deployment(&f, "openai", "env:OMG_UNSENT_TEST_KEY", None, "m", 0).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, _mock) = serve(counting(hits.clone(), StatusCode::OK)).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Kind::OpenAi.adapter(&mock_base, false))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let response = Kind::OpenAi
        .request(&base, &token, false)
        .send()
        .await
        .unwrap();
    let id: Uuid = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let rows = recorded(&f.store.pool, id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].1.as_str(), rows[0].3, rows[0].4, rows[0].5),
        ("settled", Some(0), Some(0), Some(0))
    );
}

/// An Anthropic route whose credential cannot be resolved, then an OpenAI
/// route that answers, both with an explicit `us` residency, under a policy
/// of `max_attempts` attempts and no ambiguous failover. The Anthropic mock
/// answers 401 if it is reached (it is not when its credential is refused).
async fn failover_world(
    pool: PgPool,
    max_attempts: i32,
    first_resolvable: bool,
) -> (World, Arc<AtomicUsize>) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let first = add_deployment(
        &f,
        "anthropic",
        Kind::Anthropic.credential(first_resolvable),
        None,
        "first-model",
        0,
    )
    .await;
    let second = add_deployment(
        &f,
        "openai",
        "env:OMG_UNSENT_TEST_KEY",
        None,
        "second-model",
        1,
    )
    .await;
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,$2,false)")
        .bind(f.model)
        .bind(max_attempts)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in [(first, 0), (second, 1)] {
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    let first_hits = Arc::new(AtomicUsize::new(0));
    let (first_base, first_mock) =
        serve(counting(first_hits.clone(), StatusCode::UNAUTHORIZED)).await;
    let second_hits = Arc::new(AtomicUsize::new(0));
    let counted = second_hits.clone();
    let (second_base, second_mock) = serve(Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async {
            axum::Json(json!({
                "id":"chatcmpl-1","object":"chat.completion","created":0,"model":"second-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
            }))
        }
    }))
    .await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Kind::Anthropic.adapter(&first_base, first_resolvable))
        .unwrap();
    registry
        .register(Kind::OpenAi.adapter(&second_base, true))
        .unwrap();
    let (base, gateway) = start_gateway(&f, registry).await;
    (
        World {
            f,
            token,
            base,
            hits: first_hits,
            _servers: vec![first_mock, second_mock, gateway],
        },
        second_hits,
    )
}

#[dbtest::test]
async fn a_route_that_sent_nothing_fails_over_under_the_explicit_policy(pool: PgPool) {
    let (w, second_hits) = failover_world(pool, 2, false).await;
    let (status, id, body) = call(&w, Kind::OpenAi, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    let rows = recorded(&w.f.store.pool, id).await;
    assert_eq!(
        rows.len(),
        2,
        "each attempt has its own reservation: {rows:?}"
    );
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3),
        ("failed", "settled", Some(0))
    );
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1.as_str(), rows[1].4, rows[1].5),
        ("succeeded", "settled", Some(3), Some(2))
    );
    assert!(rows[1].3.is_some_and(|a| a > 0));
}

#[dbtest::test]
async fn one_attempt_allowed_means_no_failover_even_when_nothing_was_sent(pool: PgPool) {
    let (w, second_hits) = failover_world(pool, 1, false).await;
    let (status, id, body) = call(&w, Kind::OpenAi, false).await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
    let rows = recorded(&w.f.store.pool, id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("settled", Some(0)));
}

#[dbtest::test]
async fn the_same_class_after_the_request_was_sent_does_not_fail_over(pool: PgPool) {
    // The Anthropic route resolves its credential and the upstream answers
    // 401: `Configuration`, sent, so neither a known zero nor a failover.
    let (w, second_hits) = failover_world(pool, 2, true).await;
    let (status, id, body) = call(&w, Kind::OpenAi, false).await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(w.hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
    let rows = recorded(&w.f.store.pool, id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("unknown", None));
}

/// The non-generation path (`Engine::execute_workload`): an embeddings
/// request whose credential cannot be resolved settles at zero like a chat
/// request, input-only usage included.
#[dbtest::test]
async fn an_embeddings_request_that_sent_nothing_settles_as_a_known_zero(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['embeddings']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    add_deployment(&f, "openai", "env:OMG_UNSENT_TEST_KEY", None, "embed", 0).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, _mock) = serve(counting(hits.clone(), StatusCode::OK)).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Kind::OpenAi.adapter(&mock_base, false))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/embeddings"))
        .bearer_auth(&token)
        .json(&json!({"model":"company/smart","input":"hi"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let id: Uuid = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(!status.is_success(), "{status} {body}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let rows = recorded(&f.store.pool, id).await;
    assert_eq!(rows.len(), 1, "{body}");
    let (execution, reservation, held, actual, input, output, ledger) = &rows[0];
    assert_eq!(execution, "failed");
    assert!(held.is_some_and(|h| h > 0));
    assert_eq!(
        (reservation.as_str(), *actual, *input, *output),
        ("settled", Some(0), Some(0), Some(0))
    );
    assert_eq!(
        ledger,
        &vec![("hold".into(), *held), ("settlement".into(), Some(0))]
    );
}

/// A pricing v3 version that charges per request as well as per token (its
/// admission holds no finite amount without unit ceilings): an attempt that
/// sent nothing is still a zero, with no components (they would value the
/// request line), not a one-request charge or an unknown.
#[dbtest::test]
async fn a_per_request_price_line_is_not_charged_when_nothing_was_sent(pool: PgPool) {
    use crate::billing::v3::Meter;
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment =
        add_unpriced_deployment(&f, "openai", "env:OMG_UNSENT_TEST_KEY", None, "m", 0).await;
    let lines: Vec<serde_json::Value> = Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputTokens | Meter::OutputTokens | Meter::Requests => {
                let batch = m.batches()[0];
                let rate = if m == Meter::Requests { "1000" } else { "1000000" };
                json!({"meter":m.as_str(),"microusd_per_batch":rate,"batch":batch,"unit_label":m.unit_label(batch).unwrap(),"sku_label":"Priced"})
            }
            m => json!({"meter":m.as_str(),"not_applicable":true}),
        })
        .collect();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,1000,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let (mock_base, _mock) = serve(counting(hits.clone(), StatusCode::OK)).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Kind::OpenAi.adapter(&mock_base, false))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let w = World {
        f,
        token,
        base,
        hits,
        _servers: vec![_mock, _gateway],
    };
    let (status, id, body) = call(&w, Kind::OpenAi, false).await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    let rows = recorded(&w.f.store.pool, id).await;
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(
        (rows[0].1.as_str(), rows[0].3, rows[0].4, rows[0].5),
        ("settled", Some(0), Some(0), Some(0))
    );
    let components: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT r.cost_components FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.root_request_id=$1",
    )
    .bind(id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(components, None);
}
