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
//! So that error alone is tried again, once a second, for at most
//! [`WINDOW`]; every other failure (a wrong password, which PgBouncer also
//! reports as 08P01 but with another message, a refused role, an unreachable
//! host after sqlx's own retries) fails at once, as before. A login that
//! keeps failing fails after the window with the same error.
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

/// PgBouncer's cached server-login failure (see the module documentation).
pub(crate) fn pooler_reports_failed_server_login(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e)
        if e.code().as_deref() == Some("08P01")
            && e.message().starts_with("server login has been failing"))
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
            Err(error)
                if pooler_reports_failed_server_login(&error) && Instant::now() < deadline =>
            {
                tracing::warn!(
                    "the database pooler still reports a failed server login from before PostgreSQL accepted connections; trying again"
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
