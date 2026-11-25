//! `serve`'s connections warmed before it listens.
//!
//! A new PostgreSQL backend pays for its first executions of each statement:
//! the client's Parse round trip, and planning with cold catalog caches. In
//! a burst on one scope, much of that falls inside the section in which the
//! scope's rows are locked and admissions run one after another, so a new
//! gateway's first burst was about twice as slow as a later one and the
//! admission bound shed part of it (#859).
//!
//! So while `serve` opens its pool, every connection prepares the statements
//! one admission and one settlement run, with the parameter types the hot
//! path binds (sqlx caches a prepared statement by its text alone, so the
//! types must be the same), and plans each with `EXPLAIN (GENERIC_PLAN)`
//! (PostgreSQL 16 or newer). Nothing is executed: Parse and `EXPLAIN`
//! without `ANALYZE` read and write no rows, take no row or advisory lock
//! (the functions the statements call are planned, never run) and hold only
//! the relation locks planning takes (`AccessShareLock`, and
//! `RowExclusiveLock` on an `INSERT` or `UPDATE` target, the locks the hot
//! path itself takes), each for its one statement, in the implicit
//! transaction that ends with it.
//!
//! Best effort: a server or pooler that refuses a statement, or the deadline,
//! ends the warm-up with one warning and startup goes on.
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

use gateway_datetime_compat::UtcInstant;
use sqlx::{Executor, PgPool, Postgres, Type, postgres::PgTypeInfo};
use uuid::Uuid;

/// How long the whole warm-up may take; it also ends with the startup
/// window ([`crate::db::startup::WINDOW`]).
pub(crate) const WITHIN: Duration = Duration::from_secs(5);

/// One statement of the hot path and the parameter types it is bound with.
pub(crate) struct Hot {
    pub(crate) sql: Cow<'static, str>,
    pub(crate) parameters: Vec<PgTypeInfo>,
}

fn t<T: Type<Postgres>>() -> PgTypeInfo {
    T::type_info()
}

/// The statements one scoped admission and its settlement prepare on a
/// fresh connection (`db::warm_tests` holds the two to the same set), in
/// the order a request runs them.
pub(crate) fn statements() -> Vec<Hot> {
    let uuid = t::<Uuid>;
    let text = t::<String>;
    let int = t::<i32>;
    let bigint = t::<i64>;
    let bool = t::<bool>;
    let json = t::<serde_json::Value>;
    let at = t::<UtcInstant>;
    let uuids = t::<Vec<Uuid>>;
    let ats = t::<Vec<UtcInstant>>;
    let hot = |sql: Cow<'static, str>, parameters: Vec<PgTypeInfo>| Hot { sql, parameters };
    vec![
        // Route lookup: live revalidation, deployments, policy, candidates.
        hot(
            Cow::Borrowed(crate::auth::ADMISSION_SQL_UNLOCKED.as_str()),
            vec![uuid(), uuid(), uuid()],
        ),
        hot(
            Cow::Borrowed(crate::inference::repository::ROUTE_DEPLOYMENTS_SQL),
            vec![uuid(), text(), uuid()],
        ),
        hot(
            Cow::Borrowed(crate::routing::POLICY_SQL),
            vec![uuid(), text()],
        ),
        hot(
            Cow::Borrowed(crate::routing::CANDIDATES_SQL),
            vec![uuid(), text(), uuids()],
        ),
        // Admission.
        hot(
            Cow::Borrowed(crate::governance::locks::ADMISSION_LOCKS_SQL),
            vec![uuid(), uuid(), uuid()],
        ),
        hot(
            Cow::Owned(crate::governance::admission_deployment_sql("")),
            vec![uuid(), uuid(), text(), uuid()],
        ),
        hot(
            Cow::Borrowed(crate::governance::PRICE_BY_ID_SQL.as_str()),
            vec![uuid(), uuid()],
        ),
        hot(
            Cow::Borrowed(crate::governance::locks::BOUNDED_ROWS_SQL),
            vec![uuids(), uuids(), ats(), bool(), text()],
        ),
        hot(
            Cow::Borrowed(crate::governance::rates::LIMITS_SQL.as_str()),
            vec![uuid(), uuid(), at(), bool(), bool()],
        ),
        hot(
            Cow::Borrowed(crate::governance::ADMIT_SQL),
            vec![
                uuid(),
                uuid(),
                uuid(),
                uuid(),
                text(),
                text(),
                bool(),
                at(),
                uuid(),
                int(),
                text(),
                text(),
                text(),
                text(),
                uuid(),
                at(),
                bigint(),
                bigint(),
                bool(),
                int(),
                uuid(),
            ],
        ),
        // Settlement.
        hot(
            Cow::Borrowed(crate::governance::RESERVATION_LOCKED_SQL),
            vec![uuid()],
        ),
        hot(
            Cow::Borrowed(crate::governance::locks::ROWS_SQL),
            vec![uuids(), uuids(), ats(), bool()],
        ),
        hot(
            Cow::Borrowed(crate::governance::SETTLE_SQL),
            vec![
                uuid(),
                text(),
                text(),
                bigint(),
                bigint(),
                json(),
                bigint(),
                json(),
                text(),
                bigint(),
                text(),
                bigint(),
                bigint(),
                bigint(),
                text(),
                at(),
                text(),
                bigint(),
                json(),
                bigint(),
                bool(),
                uuid(),
                text(),
                bigint(),
            ],
        ),
        // The deployment's health after the attempt.
        hot(
            Cow::Borrowed(crate::routing::HEALTH_STATE_SQL),
            vec![uuid(), int()],
        ),
        hot(
            Cow::Borrowed(crate::routing::HEALTH_RECORD_SQL),
            vec![uuid(), bool()],
        ),
    ]
}

/// Warm every connection of `pool` (all of them open, as `serve` leaves its
/// pool) with [`statements`], at once, within `within`. Never fails: an
/// error or the deadline is logged once and the rest is skipped.
pub(crate) async fn warm(pool: &PgPool, within: Duration) {
    if within.is_zero() {
        return;
    }
    let started = Instant::now();
    let connections = pool.size();
    match tokio::time::timeout(within, warm_with(pool, statements())).await {
        Ok(Ok(statements)) => tracing::info!(
            connections,
            statements,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "warmed the database connections"
        ),
        Ok(Err(error)) => tracing::warn!(
            cause = %error,
            "could not warm the database connections; starting without"
        ),
        Err(_) => tracing::warn!(
            within_ms = u64::try_from(within.as_millis()).unwrap_or(u64::MAX),
            "warming the database connections ran out of time; starting without the rest"
        ),
    }
}

/// [`warm`]'s work: each of `pool`'s connections prepares and plans
/// `statements`, all connections at once. Returns how many statements each
/// connection warmed.
pub(crate) async fn warm_with(pool: &PgPool, statements: Vec<Hot>) -> sqlx::Result<usize> {
    let count = statements.len();
    let statements = std::sync::Arc::new(statements);
    futures_util::future::try_join_all(
        (0..pool.size()).map(|_| warm_one(pool.clone(), statements.clone())),
    )
    .await?;
    Ok(count)
}

/// One connection of `pool` prepares and plans `statements`. Boxed, so the
/// future is `Send` for every caller (a `serve` task, a multi-threaded
/// test runtime) at one concrete type.
fn warm_one(
    pool: PgPool,
    statements: std::sync::Arc<Vec<Hot>>,
) -> futures_util::future::BoxFuture<'static, sqlx::Result<()>> {
    Box::pin(async move {
        let mut connection = pool.acquire().await?;
        for hot in statements.iter() {
            let conn: &mut sqlx::PgConnection = &mut connection;
            Executor::prepare_with(conn, hot.sql.as_ref(), &hot.parameters).await?;
            let explain = format!("EXPLAIN (GENERIC_PLAN) {}", hot.sql);
            let conn: &mut sqlx::PgConnection = &mut connection;
            Executor::execute(conn, sqlx::raw_sql(&explain)).await?;
        }
        Ok(())
    })
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "warm_tests.rs"]
mod tests;
