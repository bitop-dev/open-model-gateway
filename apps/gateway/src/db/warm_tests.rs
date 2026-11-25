//! The warm-up prepares exactly what one admission and one settlement
//! prepare on a fresh connection, executes nothing, and never fails startup.
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use super::*;
use crate::{
    governance::tests::db::{fixture, request},
    inference::{
        Engine, EngineLimits,
        error::InferenceError,
        types::{
            Capabilities, ChatRequest, ChatResponse, Deployment, FinishReason, ProviderOutput,
            Usage,
        },
    },
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};

struct Upstream;
#[async_trait]
impl ProviderAdapter for Upstream {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Ok(ProviderOutput::Complete(ChatResponse {
            messages_content: None,
            reasoning: None,
            content: Some("ok".into()),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(1),
                ..Default::default()
            },
        }))
    }
}

/// A pool of one fresh connection to the test database.
async fn one(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

/// The statements, with their parameter types, prepared on `pool`'s only
/// connection (without the query that lists them).
async fn prepared(pool: &PgPool) -> Vec<(String, String)> {
    const LIST: &str =
        "SELECT statement,parameter_types::text FROM pg_prepared_statements WHERE from_sql=false";
    let mut statements: Vec<(String, String)> = sqlx::query_as(LIST)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|(statement, _): &(String, String)| statement != LIST)
        .collect();
    statements.sort();
    statements
}

#[dbtest::test]
async fn the_warm_up_prepares_what_one_admission_and_settlement_prepare(pool: PgPool) {
    let fixture = fixture(pool.clone()).await;
    fixture.price(1_000_000).await;
    // One request on a fresh connection: what the hot path prepares.
    let request_pool = one(&pool).await;
    let store = Store::new(request_pool.clone());
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(Upstream)).unwrap();
    let engine = Engine::new(Arc::new(store), registry, EngineLimits::default()).unwrap();
    engine
        .execute(fixture.principal, request(), Uuid::now_v7())
        .await
        .unwrap();
    let by_request = prepared(&request_pool).await;
    // The warm-up on another fresh connection.
    let warm_pool = one(&pool).await;
    assert_eq!(
        warm_with(&warm_pool, statements()).await.unwrap(),
        statements().len()
    );
    let by_warm_up = prepared(&warm_pool).await;
    assert_eq!(by_request.len(), statements().len(), "{by_request:#?}");
    assert_eq!(
        by_warm_up, by_request,
        "the warm-up and the hot path prepare the same statements with the same parameter types"
    );
}

/// Nothing is written, no transaction stays open and no lock stays held.
#[dbtest::test]
async fn the_warm_up_executes_nothing_and_leaves_no_transaction_or_lock(pool: PgPool) {
    let fixture = fixture(pool.clone()).await;
    fixture.price(1_000_000).await;
    const ROWS: &str = "SELECT (SELECT count(*) FROM inference_executions)+(SELECT count(*) FROM governance_reservations)+(SELECT count(*) FROM monetary_ledger)+(SELECT count(*) FROM budget_totals)+(SELECT count(*) FROM rate_minute_counters)+(SELECT count(*) FROM inflight_counters)+(SELECT count(*) FROM deployment_health)";
    let before: i64 = sqlx::query_scalar(ROWS).fetch_one(&pool).await.unwrap();
    let warm_pool = PgPoolOptions::new()
        .max_connections(4)
        .min_connections(4)
        .connect_lazy_with(
            (*pool.connect_options())
                .clone()
                .application_name("omg_warm_test"),
        );
    // Open all four, as `serve` leaves its pool.
    let open: Vec<_> = futures_util::future::try_join_all((0..4).map(|_| warm_pool.acquire()))
        .await
        .unwrap();
    drop(open);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while warm_pool.num_idle() < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    warm(&warm_pool, WITHIN).await;
    let after: i64 = sqlx::query_scalar(ROWS).fetch_one(&pool).await.unwrap();
    assert_eq!(after, before, "the warm-up wrote nothing");
    let (sessions, in_transaction, locks): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE xact_start IS NOT NULL OR state<>'idle'),
           (SELECT count(*) FROM pg_locks l WHERE l.pid IN (SELECT pid FROM pg_stat_activity WHERE application_name='omg_warm_test'))
         FROM pg_stat_activity WHERE application_name='omg_warm_test'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sessions, 4);
    assert_eq!(
        (in_transaction, locks),
        (0, 0),
        "warmed connections are idle and hold no lock"
    );
}

/// A statement the server refuses ends the warm-up with its error, never a
/// panic or a broken pool; `warm` itself only logs it.
#[dbtest::test]
async fn a_refused_statement_ends_the_warm_up_without_failing(pool: PgPool) {
    let warm_pool = one(&pool).await;
    let refused = vec![Hot {
        sql: Cow::Borrowed("SELECT no_such_column FROM no_such_table WHERE id=$1"),
        parameters: vec![t::<Uuid>()],
    }];
    assert!(warm_with(&warm_pool, refused).await.is_err());
    let one_row: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&warm_pool)
        .await
        .unwrap();
    assert_eq!(one_row, 1, "the pool still works");
}
