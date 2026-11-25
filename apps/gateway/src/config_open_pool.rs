//! `serve`'s pool, opened whole before it listens (`Config::connect_serving`).
use std::time::Duration;

use anyhow::{Context, Result, bail};

impl super::Config {
    /// The primary pool of `serve`: all `GATEWAY_DATABASE_MAX_CONNECTIONS`
    /// connections opened before the gateway listens (so before it is
    /// ready), and kept open (`min_connections`), so a replica that joins
    /// under load never opens connections while its first requests wait for
    /// them. A database that cannot give each one within the acquire timeout
    /// (3 s; all of them within [`OPEN_POOL_TIMEOUT`]) fails startup, as an
    /// unreachable one does. One-shot commands keep the
    /// pool that opens connections when needed ([`Config::connect`]).
    pub async fn connect_serving(&self) -> Result<sqlx::PgPool> {
        // Lazily, then all at once: a pool built with `connect` would open
        // its minimum one connection after another inside one acquire
        // timeout.
        let pool = self
            .primary_pool_options()
            .min_connections(self.database_max_connections)
            .connect_lazy(&self.database_url)
            .context("DATABASE_URL is not a valid PostgreSQL URL")?;
        open_pool(&pool, self.database_max_connections, OPEN_POOL_TIMEOUT)
            .await
            .context("could not connect to PostgreSQL")?;
        Ok(pool)
    }
}

/// The outer bound on opening `serve`'s pool at startup; each connection is
/// also bounded by the pool's acquire timeout (`ACQUIRE_TIMEOUT`, 3 s), and
/// they open at once.
pub(crate) const OPEN_POOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Open `connections` connections of `pool` at once (each acquire opens one
/// while fewer are open), within `within`, and return them to it.
pub(crate) async fn open_pool(
    pool: &sqlx::PgPool,
    connections: u32,
    within: Duration,
) -> Result<()> {
    let opening = futures_util::future::try_join_all((0..connections).map(|_| pool.acquire()));
    match tokio::time::timeout(within, opening).await {
        Ok(Ok(open)) => {
            drop(open);
            Ok(())
        }
        Ok(Err(error)) => Err(error).with_context(|| {
            format!("could not open the database pool's {connections} connections (GATEWAY_DATABASE_MAX_CONNECTIONS)")
        }),
        Err(_) => bail!(
            "could not open the database pool's {connections} connections (GATEWAY_DATABASE_MAX_CONNECTIONS) within {}s",
            within.as_secs()
        ),
    }
}

/// `serve` opens its whole pool before it listens: every connection is
/// open and idle afterwards, and a database that cannot give them in time
/// refuses startup.
#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    #[dbtest::test]
    async fn serve_opens_every_connection_of_its_pool_before_listening(pool: sqlx::PgPool) {
        let serving = PgPoolOptions::new()
            .max_connections(4)
            .min_connections(4)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        open_pool(&serving, 4, OPEN_POOL_TIMEOUT).await.unwrap();
        assert_eq!(serving.size(), 4);
        // Returned to the pool (asynchronously): all four idle and ready.
        tokio::time::timeout(Duration::from_secs(5), async {
            while serving.num_idle() < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let lazy = PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy_with((*pool.connect_options()).clone());
        let error = open_pool(&lazy, 4, Duration::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("within 0s"), "{error}");
    }
}
