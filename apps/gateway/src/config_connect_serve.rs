//! `serve`'s primary pool (`Config::connect_serve`): every connection open
//! before the gateway listens, through [`crate::db::startup`].
use anyhow::{Context, Result};

impl super::Config {
    /// `serve`'s primary pool: all `GATEWAY_DATABASE_MAX_CONNECTIONS`
    /// connections opened before the gateway listens (so before it is
    /// ready) and kept open, with a pooler's cached failed server login from
    /// before PostgreSQL accepted connections waited out
    /// ([`crate::db::startup`]). Any other failure to open them stops
    /// startup; one that ran out of time names the setting. One-shot
    /// commands keep the pool that opens connections when needed
    /// ([`Self::connect`]).
    pub async fn connect_serve(&self) -> Result<sqlx::PgPool> {
        let options =
            <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(&self.database_url)
                .context("could not connect to PostgreSQL")?;
        crate::db::startup::connect(
            self.primary_pool_options(),
            options,
            crate::db::startup::WINDOW,
        )
        .await
        .map_err(|error| {
            let connections = self.database_max_connections;
            // Out of connection slots at the database or the pooler (or a
            // database still starting, or not answering, which sqlx also
            // ends with a timeout): the setting is the one to check.
            let context = if crate::db::startup::out_of_connections(&error) {
                format!(
                    "could not open the database pool's {connections} connections (GATEWAY_DATABASE_MAX_CONNECTIONS): the database or pooler is out of connection slots, still starting, or not answering"
                )
            } else {
                format!("could not open the database pool's {connections} connections")
            };
            anyhow::Error::new(error).context(context)
        })
        .context("could not connect to PostgreSQL")
    }
}
