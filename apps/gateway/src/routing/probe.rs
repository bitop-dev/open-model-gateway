//! The probe slot of a half-open route (#49): one real request decides
//! whether traffic resumes, one prober per route across replicas. Each
//! function is one statement on the route's health row, outside any
//! admission lock (the health row is no authority row).
use uuid::Uuid;

use crate::{inference::error::InferenceError, store::Store};

/// The longest `Retry-After` (seconds) for a route whose probe slot another
/// request holds: the slot can last the request timeout, but a probe usually
/// ends in seconds, and a client told to wait out the whole slot would stay
/// away long after the route recovered. An open circuit reports its own
/// remaining cooldown.
pub const PROBE_RETRY_AFTER_SECONDS: i32 = 5;

/// The health row's route (`deployment_health.deployment_id`) is not
/// drained now (`routing::drain`).
static NOT_DRAINED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "NOT EXISTS(SELECT 1 FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=deployment_health.deployment_id AND {})",
        super::drain::ROUTE_DRAINED
    )
});

/// The outcome of [`claim_probe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeClaim {
    /// This request holds the probe slot under this epoch.
    Claimed(i64),
    /// The route is no longer half-open: its circuit closed meanwhile.
    Closed,
    /// Another request holds the slot, or the circuit opened again: the
    /// route is cooling down for about this many seconds.
    Busy(u32),
}

/// Claim the probe slot of a half-open route for one request (`holder`), for
/// at most `hold`: the request's own bound (its timeout and its settlement
/// deadline), after which the slot ends on its own. One conditional update,
/// made only for a route the plan found half-open.
pub async fn claim_probe(
    store: &Store,
    deployment: Uuid,
    holder: Uuid,
    hold: std::time::Duration,
) -> Result<ProbeClaim, InferenceError> {
    // A drained route is never claimed (`routing::drain`); it reads as busy.
    let sql = format!(
        "UPDATE deployment_health SET probe_holder=$2, probe_until=clock_timestamp()+make_interval(secs => $3), probe_epoch=probe_epoch+1
         WHERE deployment_id=$1 AND open_until <= statement_timestamp()
           AND (probe_until IS NULL OR probe_until <= statement_timestamp())
           AND {not_drained}
         RETURNING probe_epoch",
        not_drained = NOT_DRAINED.as_str()
    );
    let claimed: Option<i64> = sqlx::query_scalar(&sql)
        .bind(deployment)
        .bind(holder)
        .bind(hold.as_secs_f64())
        .fetch_optional(&store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?;
    store.caches.forget_health(deployment);
    if let Some(epoch) = claimed {
        return Ok(ProbeClaim::Claimed(epoch));
    }
    let wait: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT CASE WHEN open_until IS NULL THEN NULL
                     WHEN open_until > clock_timestamp()
                     THEN LEAST(86400, GREATEST(1, CEIL(EXTRACT(EPOCH FROM open_until - clock_timestamp()))))::int
                     ELSE LEAST($2, GREATEST(1, CEIL(EXTRACT(EPOCH FROM probe_until - clock_timestamp()))))::int END
         FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(deployment)
    .bind(PROBE_RETRY_AFTER_SECONDS)
    .fetch_optional(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?;
    Ok(match wait.flatten() {
        None => ProbeClaim::Closed,
        Some(seconds) => ProbeClaim::Busy(seconds.max(1) as u32),
    })
}

/// A slot claimed earlier (a batch line claims before it waits for route
/// capacity) is renewed when its request is sent, for that request's own
/// bound from now. `false`: the slot is no longer this request's (it
/// expired, was released, or a newer probe holds it), and the request must
/// not be sent as the probe.
pub async fn renew_probe(
    store: &Store,
    deployment: Uuid,
    epoch: i64,
    hold: std::time::Duration,
) -> Result<bool, InferenceError> {
    // A route drained since the claim is no longer the caller's to send on.
    let sql = format!(
        "UPDATE deployment_health SET probe_until=clock_timestamp()+make_interval(secs => $3)
         WHERE deployment_id=$1 AND probe_epoch=$2 AND probe_holder IS NOT NULL
           AND probe_until > statement_timestamp() AND {not_drained}",
        not_drained = NOT_DRAINED.as_str()
    );
    let renewed = sqlx::query(&sql)
        .bind(deployment)
        .bind(epoch)
        .bind(hold.as_secs_f64())
        .execute(&store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?
        .rows_affected()
        == 1;
    store.caches.forget_health(deployment);
    Ok(renewed)
}

/// The probe's stream delivered its first content: the route is serving, so
/// the circuit closes and the slot is freed at once; the openings are
/// forgotten only by a complete success (`record_result`). Nothing changes
/// unless this request still holds the slot under `epoch`.
pub async fn close_probe(
    store: &Store,
    deployment: Uuid,
    epoch: i64,
) -> Result<(), InferenceError> {
    // On a drained route the first content decides nothing either; the slot
    // is freed when the probe ends.
    let sql = format!(
        "UPDATE deployment_health SET consecutive_failures=0, open_until=NULL, probe_holder=NULL, probe_until=NULL, last_observed_at=clock_timestamp()
         WHERE deployment_id=$1 AND probe_epoch=$2 AND probe_holder IS NOT NULL AND {not_drained}",
        not_drained = NOT_DRAINED.as_str()
    );
    sqlx::query(&sql)
        .bind(deployment)
        .bind(epoch)
        .execute(&store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?;
    store.caches.forget_health(deployment);
    Ok(())
}

/// The probe ended without a decision (cancelled, refused before the
/// provider, or an outcome that is no health signal): free the slot for the
/// next request. Nothing changes unless this request still holds it under
/// `epoch`.
pub async fn release_probe(
    store: &Store,
    deployment: Uuid,
    epoch: i64,
) -> Result<(), InferenceError> {
    sqlx::query(
        "UPDATE deployment_health SET probe_holder=NULL, probe_until=NULL
         WHERE deployment_id=$1 AND probe_epoch=$2 AND probe_holder IS NOT NULL",
    )
    .bind(deployment)
    .bind(epoch)
    .execute(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?;
    store.caches.forget_health(deployment);
    Ok(())
}
