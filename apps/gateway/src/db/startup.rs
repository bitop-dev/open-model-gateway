//! `serve`'s first database connection, through a PgBouncer pooler that is
//! still reporting a failed server login from before PostgreSQL accepted
//! connections.
//!
//! PgBouncer answers a client with SQLSTATE 08P01 "server login has been
//! failing, cached error: … (server_login_retry)" while its pool's last
//! server login failed and it holds no server connection (PgBouncer 1.25.2,
//! `src/objects.c`), for `server_login_retry` (15 s by default) after that
//! failure. At a first install every Pooler instance tries PostgreSQL before
//! it is up, so one can still be in that window when another already serves:
//! the gateway's `wait-for-database --schema` init container passes through
//! one, and `serve` can then reach the other and exit. sqlx retries only a
//! refused connection and 53300/57P03 within its acquire timeout.
//!
//! So that error alone is tried again, once a second, while the next attempt
//! would start within [`WINDOW`]; every other failure (a wrong password,
//! which PgBouncer also reports as 08P01 but with another message, an
//! unreachable host after sqlx's own retries) fails at once, as before.
//! PgBouncer gives the same cached answer when PostgreSQL itself refuses the
//! pooler's login (a revoked CONNECT, a pg_hba rule, a role connection
//! limit), so such a refusal is tried for the window too and then fails with
//! that error: delayed by at most the window and one attempt's acquire
//! timeout, never hidden.
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

/// Connect `pool` with `options`, trying again while the pooler reports a
/// cached failed server login, until `window` has passed.
pub(crate) async fn connect(
    pool: PgPoolOptions,
    options: PgConnectOptions,
    window: Duration,
) -> sqlx::Result<PgPool> {
    let deadline = Instant::now() + window;
    loop {
        match pool.clone().connect_with(options.clone()).await {
            Err(error) if Instant::now() + INTERVAL < deadline => {
                let Some(reason) = cached_reason(&error) else {
                    return Err(error);
                };
                tracing::warn!(
                    reason,
                    "the database pooler still reports a failed server login; trying again"
                );
                tokio::time::sleep(INTERVAL).await;
            }
            other => return other,
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "startup_tests.rs"]
mod tests;
