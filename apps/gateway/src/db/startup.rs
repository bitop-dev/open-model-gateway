//! `serve`'s primary pool: every connection opened before the gateway
//! listens, through a PgBouncer pooler that may still report a failed server
//! login from before PostgreSQL accepted connections.
//!
//! The whole pool is opened at startup and kept open (`min_connections`
//! equal to the maximum), so a replica that joins under load never opens
//! connections while its first requests wait for them. They are opened at
//! once (a pool built with `connect` would open its minimum one after
//! another inside one acquire timeout); each is bounded by the pool's
//! acquire timeout and an attempt by [`OPEN_WITHIN`].
//!
//! PgBouncer answers a client with SQLSTATE 08P01 "server login has been
//! failing, cached error: … (server_login_retry)" while its pool's last
//! server login failed and it holds no server connection (PgBouncer 1.25.2,
//! `src/objects.c`), for `server_login_retry` (15 s by default) after that
//! failure. At a first install every Pooler instance tries PostgreSQL before
//! it is up, so one can still be in that window when another already serves:
//! the gateway's `wait-for-database --schema` init container passes through
//! one, and some of `serve`'s connections can then reach the other. sqlx
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

/// The outer bound on one attempt to open the whole pool. Each connection is
/// already bounded by the pool's acquire timeout (`ACQUIRE_TIMEOUT`, 3 s),
/// and they open at once.
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

/// The pool of `pool` with `options`, with all of its maximum connections
/// open, trying again while the pooler reports a cached failed server login,
/// until `window` has passed. On an error the pool is closed.
pub(crate) async fn connect(
    pool: PgPoolOptions,
    options: PgConnectOptions,
    window: Duration,
) -> sqlx::Result<PgPool> {
    let connections = pool.get_max_connections();
    let pool = pool.min_connections(connections).connect_lazy_with(options);
    let deadline = Instant::now() + window;
    loop {
        let error = match open(&pool, connections).await {
            Ok(()) => return Ok(pool),
            Err(error) => error,
        };
        let reason = cached_reason(&error);
        match reason {
            Some(reason) if Instant::now() + INTERVAL < deadline => {
                tracing::warn!(
                    reason,
                    open = pool.size(),
                    connections,
                    "the database pooler still reports a failed server login; trying again"
                );
                tokio::time::sleep(INTERVAL).await;
            }
            _ => {
                pool.close().await;
                return Err(error);
            }
        }
    }
}

/// Acquire `connections` connections of `pool` at once (each acquire opens
/// one while fewer are open) within [`OPEN_WITHIN`], and return them to it.
async fn open(pool: &PgPool, connections: u32) -> sqlx::Result<()> {
    let opening = futures_util::future::try_join_all((0..connections).map(|_| pool.acquire()));
    match tokio::time::timeout(OPEN_WITHIN, opening).await {
        Ok(Ok(open)) => {
            drop(open);
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(sqlx::Error::PoolTimedOut),
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "startup_tests.rs"]
mod tests;
