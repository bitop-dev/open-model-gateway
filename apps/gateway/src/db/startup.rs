//! `serve`'s primary pool: every connection opened before the gateway
//! listens, through a PgBouncer pooler that may still report a failed server
//! login from before PostgreSQL accepted connections.
//!
//! The whole pool is opened at startup and kept open (`min_connections`
//! equal to the maximum), so a replica that joins under load never opens
//! connections while its first requests wait for them. They are opened at
//! once (a pool built with `connect` would open its minimum one after
//! another inside one acquire timeout), and each counts as open once it has
//! answered a query; each acquire is bounded by the pool's acquire timeout
//! and an attempt by [`OPEN_WITHIN`].
//!
//! PgBouncer answers a client with SQLSTATE 08P01 "server login has been
//! failing, cached error: … (server_login_retry)" while its pool's last
//! server login failed and it holds no server connection (PgBouncer 1.25.2,
//! `src/objects.c`), for `server_login_retry` (15 s by default) after that
//! failure. At a first install every Pooler instance tries PostgreSQL before
//! it is up, so one can still be in that window when another already serves:
//! the gateway's `wait-for-database --schema` init container passes through
//! one, and some of `serve`'s connections can then reach the other. A pool
//! that has served (PgBouncer 1.25.2) completes the client's login from its
//! cached server parameters and sends that error at the first query; one
//! that never logged in holds new clients until its next login attempt. sqlx
//! retries only a refused connection and 53300/57P03 within its acquire
//! timeout.
//!
//! So that error alone is tried again, once a second, while the next attempt
//! would start within [`WINDOW`]: the connections already open stay in the
//! pool and the next attempt opens the rest. Every other failure (a wrong
//! password, which PgBouncer also reports as 08P01 but with another message;
//! an unreachable host or a database out of connection slots after sqlx's
//! own retries) fails at once. PgBouncer gives the same cached answer when
//! PostgreSQL itself refuses the pooler's login (a revoked CONNECT, a
//! pg_hba rule, a role connection limit), so such a refusal is tried for the
//! window too and then fails with that error: delayed by at most the window
//! and one attempt, never hidden.
use std::time::{Duration, Instant};

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

/// Twice PgBouncer's default `server_login_retry`: a Pooler instance that
/// failed its last server login before PostgreSQL was up has tried again by
/// then.
pub(crate) const WINDOW: Duration = Duration::from_secs(30);
const INTERVAL: Duration = Duration::from_secs(1);

/// The bound on one attempt to open the whole pool: each connection's
/// acquire is bounded by the pool's acquire timeout (`ACQUIRE_TIMEOUT`, 3 s,
/// in `serve`), and its first query by this (a pooler can hold a query
/// while it waits for a server connection).
pub(crate) const OPEN_WITHIN: Duration = Duration::from_secs(10);

/// PgBouncer's cached server-login failure (see the module documentation):
/// the cause it cached ("connect failed", or PostgreSQL's own message, at
/// most 128 bytes). It never holds a password, but PostgreSQL's message can
/// name a client address, the role and the database (a `pg_hba` refusal
/// does): the same text `serve` prints when it stops with that error, so
/// logging it while waiting discloses nothing more.
fn cached_reason(error: &sqlx::Error) -> Option<&str> {
    let sqlx::Error::Database(e) = error else {
        return None;
    };
    if e.code().as_deref() != Some("08P01") {
        return None;
    }
    let rest = e.message().strip_prefix("server login has been failing")?;
    Some(
        rest.strip_prefix(", cached error: ")
            .map_or(rest, |reason| {
                reason.trim_end_matches(" (server_login_retry)")
            }),
    )
}

/// How long a failed startup waits for its pool to close; the process exits
/// after the error anyway.
const CLOSE_WITHIN: Duration = Duration::from_secs(1);

/// The pool of `pool` with `options`, with all of its maximum connections
/// open, trying again while the pooler reports a cached failed server login,
/// until `window` has passed. On an error the pool is closed (within
/// [`CLOSE_WITHIN`]).
///
/// No idle timeout: with the minimum equal to the maximum, sqlx's reaper
/// would close every connection idle that long and open it again at once.
/// The maximum lifetime still recycles connections.
pub(crate) async fn connect(
    pool: PgPoolOptions,
    options: PgConnectOptions,
    window: Duration,
) -> sqlx::Result<PgPool> {
    let connections = pool.get_max_connections();
    let pool = pool
        .min_connections(connections)
        .idle_timeout(None)
        .connect_lazy_with(options);
    let deadline = Instant::now() + window;
    loop {
        let (answered, errors) = match open(&pool, connections).await {
            Ok(()) => return Ok(pool),
            Err(failed) => failed,
        };
        let cached = errors.iter().find_map(cached_reason);
        match cached {
            Some(reason) if Instant::now() + INTERVAL < deadline => {
                tracing::warn!(
                    reason,
                    answered,
                    connections,
                    "the database pooler still reports a failed server login; trying again"
                );
                tokio::time::sleep(INTERVAL).await;
            }
            _ => {
                let _ = tokio::time::timeout(CLOSE_WITHIN, pool.close()).await;
                return Err(reported(errors));
            }
        }
    }
}

/// The error startup stops with: any other than a pooler's cached failure,
/// which would otherwise hide it, else the cached failure itself.
fn reported(mut errors: Vec<sqlx::Error>) -> sqlx::Error {
    let index = errors
        .iter()
        .position(|e| cached_reason(e).is_none())
        .unwrap_or(0);
    errors.swap_remove(index)
}

/// Acquire `connections` connections of `pool` at once (each acquire opens
/// one while fewer are open), each answering a query, within
/// [`OPEN_WITHIN`], and return them to it. The query is what proves a
/// connection through PgBouncer: once a pool has served, PgBouncer completes
/// a client's login from the server parameters it cached and sends a cached
/// failed server login at the client's first query instead. Every
/// connection's attempt runs to its end, so the ones that answered stay in
/// the pool whichever failed (a refused one is closed by the pooler and
/// dropped); on a failure, how many answered and all of the errors, so a
/// slower connection's timeout never hides a pooler's cached failure.
async fn open(pool: &PgPool, connections: u32) -> Result<(), (usize, Vec<sqlx::Error>)> {
    let opening = futures_util::future::join_all((0..connections).map(|_| async {
        let mut connection = pool.acquire().await?;
        if let Err(error) = sqlx::Executor::execute(&mut *connection, "SELECT 1").await {
            // Closed by the pooler: out of the pool, never handed out again.
            connection.close_on_drop();
            return Err(error);
        }
        Ok::<_, sqlx::Error>(connection)
    }));
    let Ok(results) = tokio::time::timeout(OPEN_WITHIN, opening).await else {
        return Err((0, vec![sqlx::Error::PoolTimedOut]));
    };
    let mut errors = Vec::new();
    let mut open = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(connection) => open.push(connection),
            Err(error) => errors.push(error),
        }
    }
    let answered = open.len();
    drop(open);
    if errors.is_empty() {
        Ok(())
    } else {
        Err((answered, errors))
    }
}

/// The database or the pooler had no connection left for the pool: a
/// timeout (sqlx retries PostgreSQL's 53300 "too many connections" and
/// 57P03 "starting up" until its acquire timeout, then reports this), a
/// 53300 itself, or PgBouncer's "no more connections allowed
/// (max_client_conn)".
pub(crate) fn out_of_connections(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Database(e) => {
            e.code().as_deref() == Some("53300")
                || e.message().starts_with("no more connections allowed")
        }
        _ => false,
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "startup_tests.rs"]
mod tests;
