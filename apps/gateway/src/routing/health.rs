//! A route's passive health (#49): each recorded provider result moves its
//! circuit (closed, open for a cooldown, half-open). While the circuit is open
//! or half-open the probe's result decides, and on a half-open circuit with
//! no unexpired slot a never-claiming kind's result (realtime) does too.
use super::*;

pub(super) fn affects_health(error: Option<InferenceError>) -> bool {
    matches!(
        error,
        None | Some(
            InferenceError::Busy
                | InferenceError::Overloaded
                | InferenceError::UpstreamUnavailable
                | InferenceError::Timeout
        )
    )
}

/// How stale `last_observed_at` may get while a healthy route keeps succeeding.
const OBSERVED_REFRESH_SECONDS: i32 = 30;

/// Record an actual provider result, not local capacity/admission or accounting
/// failures. Success means validated completion (for streams: valid terminal Done),
/// not merely opening a response/stream. Updates serialize atomically in PostgreSQL.
///
/// A success on a route that is already healthy (no failures, no open
/// circuit) and was observed within the last 30 s writes nothing: the health
/// state would not change, so the per-success upsert of one hot row per
/// deployment is skipped. Failures and recoveries always write.
///
/// This is the result of a request of a kind that claims a half-open route's
/// probe slot (generation, workloads, batch lines) but did not hold it: while
/// the circuit is open or half-open it changes nothing (an attempt admitted
/// before the circuit opened, a request that was passed the route as closed).
pub async fn record_result(
    store: &Store,
    deployment: Uuid,
    error: Option<InferenceError>,
) -> Result<(), InferenceError> {
    record(store, deployment, error, Recorder::Attempt).await
}

/// [`record_result`] of the request that claimed the probe slot under
/// `epoch`: while that claim is the slot's latest and the slot has not been
/// freed, it decides the open or half-open circuit (a success closes it, a
/// counted failure reopens it, a failure below the threshold leaves it
/// half-open) and frees the slot. A claim whose slot expired with nobody
/// claiming it again still decides: it is the only request that was let
/// through. A newer claim, or a slot already freed, leaves it nothing.
pub async fn record_probe_result(
    store: &Store,
    deployment: Uuid,
    error: Option<InferenceError>,
    epoch: i64,
) -> Result<(), InferenceError> {
    record(store, deployment, error, Recorder::Probe(epoch)).await
}

/// [`record_result`] of a request of a kind that never claims the probe
/// slot (realtime sessions): it reaches a half-open route as it reached an
/// expired cooldown, so on a route only such kinds use, it is what keeps the
/// breaker working. It decides a half-open circuit while no request holds an
/// unexpired probe slot; it never changes an open circuit, and never a slot's
/// holder's decision.
pub async fn record_unclaimed_result(
    store: &Store,
    deployment: Uuid,
    error: Option<InferenceError>,
) -> Result<(), InferenceError> {
    record(store, deployment, error, Recorder::Unclaimed).await
}

/// Whose result is recorded, which decides what it may change.
#[derive(Clone, Copy)]
enum Recorder {
    Attempt,
    Probe(i64),
    Unclaimed,
}

/// A result may change the circuit: it is closed; or this is the probe's
/// result, under the slot's latest claim while the slot is not freed; or it
/// is a never-claiming kind's, on a half-open circuit with no unexpired slot.
const AUTHORITY: &str = "(h.open_until IS NULL
    OR ($3::bigint IS NOT NULL AND h.probe_epoch=$3 AND h.probe_holder IS NOT NULL)
    OR ($4 AND h.open_until <= statement_timestamp()
        AND (h.probe_holder IS NULL OR h.probe_until <= statement_timestamp())))";
/// A failure that reaches the threshold while the circuit is closed or
/// half-open opens it (one statement time for every column).
const OPENING: &str = "($2 AND h.consecutive_failures::bigint+1 >= (SELECT threshold FROM policy) AND (h.open_until IS NULL OR h.open_until <= statement_timestamp()))";

static RECORD_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        r#"WITH policy AS (
            SELECT COALESCE(p.failure_threshold,3) AS threshold,
                   COALESCE(p.cooldown_seconds,30) AS cooldown
            FROM deployments d LEFT JOIN deployment_routing p
              ON p.deployment_id=d.id
            WHERE d.id=$1
        )
        INSERT INTO deployment_health AS h
            (deployment_id, consecutive_failures, open_until, last_observed_at, reopen_count)
        SELECT $1, CASE WHEN $2 THEN 1 ELSE 0 END,
            CASE WHEN $2 AND threshold <= 1 THEN clock_timestamp() + make_interval(secs => cooldown) ELSE NULL END,
            clock_timestamp(), CASE WHEN $2 AND threshold <= 1 THEN 1 ELSE 0 END FROM policy
        ON CONFLICT (deployment_id) DO UPDATE SET
            consecutive_failures=CASE
                WHEN NOT {AUTHORITY} THEN h.consecutive_failures
                WHEN $2 THEN LEAST(h.consecutive_failures::bigint+1,2147483647)::integer ELSE 0 END,
            open_until=CASE
                WHEN NOT {AUTHORITY} THEN h.open_until
                WHEN NOT $2 THEN NULL
                WHEN {OPENING} THEN clock_timestamp() + make_interval(secs => (SELECT LEAST(
                    cooldown::double precision * power(2, LEAST(h.reopen_count, 20)),
                    GREATEST(300, cooldown)) FROM policy))
                ELSE h.open_until END,
            reopen_count=CASE
                WHEN NOT {AUTHORITY} THEN h.reopen_count
                WHEN NOT $2 THEN 0
                WHEN {OPENING} THEN LEAST(h.reopen_count::bigint+1,2147483647)::integer
                ELSE h.reopen_count END,
            probe_holder=CASE WHEN {AUTHORITY} AND (NOT $2 OR {OPENING} OR h.open_until IS NOT NULL) THEN NULL ELSE h.probe_holder END,
            probe_until=CASE WHEN {AUTHORITY} AND (NOT $2 OR {OPENING} OR h.open_until IS NOT NULL) THEN NULL ELSE h.probe_until END,
            last_observed_at=clock_timestamp()"#
    )
});

async fn record(
    store: &Store,
    deployment: Uuid,
    error: Option<InferenceError>,
    recorder: Recorder,
) -> Result<(), InferenceError> {
    if !affects_health(error) {
        return Ok(());
    }
    if error.is_none()
        && store.caches.enabled()
        && store.caches.health_entry(deployment).is_some_and(|h| {
            h.consecutive_failures == 0
                && h.open_until.is_none()
                && h.reopen_count == 0
                && h.observed_age.is_some_and(|age| {
                    age + h.loaded.elapsed()
                        < std::time::Duration::from_secs(OBSERVED_REFRESH_SECONDS as u64)
                })
        })
    {
        // Healthy and recently observed as of at most `HEALTH_TTL` ago: the
        // write would change nothing (the same staleness the planner accepts).
        return Ok(());
    }
    if error.is_none() {
        // Healthy: no failures, no circuit and no openings left to forget (a
        // complete success resets the reopen count).
        let state: Option<(bool, bool)> = sqlx::query_as("SELECT consecutive_failures=0 AND open_until IS NULL AND reopen_count=0, last_observed_at>clock_timestamp()-$2*interval '1 second' FROM deployment_health WHERE deployment_id=$1")
            .bind(deployment)
            .bind(OBSERVED_REFRESH_SECONDS)
            .fetch_optional(&store.pool)
            .await
            .map_err(|_| InferenceError::Storage)?;
        match state {
            Some((true, true)) => return Ok(()),
            Some((true, false)) => {
                // Healthy but stale: only the timestamp changes. One task per
                // deployment and replica refreshes it, and never queues on a
                // busy row (a slow commit elsewhere): the others return at
                // once instead of each holding a pooled connection (#132).
                let Some(_claim) = store.caches.claim_health_refresh(deployment) else {
                    return Ok(());
                };
                let refreshed = sqlx::query("UPDATE deployment_health SET last_observed_at=clock_timestamp() WHERE deployment_id=(SELECT deployment_id FROM deployment_health WHERE deployment_id=$1 AND consecutive_failures=0 AND open_until IS NULL AND reopen_count=0 FOR UPDATE SKIP LOCKED)")
                    .bind(deployment)
                    .execute(&store.pool)
                    .await
                    .map_err(|_| InferenceError::Storage)?
                    .rows_affected();
                if refreshed == 1 {
                    store.caches.forget_health(deployment);
                    return Ok(());
                }
                // Busy (advisory; the next success retries), unless a failure
                // committed meanwhile: then this success must end the streak.
                let healthy: Option<bool> = sqlx::query_scalar("SELECT consecutive_failures=0 AND open_until IS NULL AND reopen_count=0 FROM deployment_health WHERE deployment_id=$1")
                    .bind(deployment)
                    .fetch_optional(&store.pool)
                    .await
                    .map_err(|_| InferenceError::Storage)?;
                if healthy == Some(true) {
                    return Ok(());
                }
            }
            // First observation, or a recovery that must end a failure streak.
            _ => {}
        }
    }
    // A success closes the circuit, frees the slot and forgets the openings;
    // a counted failure at the threshold opens it for the deployment's
    // cooldown doubled once per earlier opening, capped at 300 s (or the
    // cooldown, if longer), and ends any probe slot; a decision on an open or
    // half-open circuit always frees the slot. None of this applies without
    // the authority to change the circuit.
    let (epoch, unclaimed) = match recorder {
        Recorder::Attempt => (None, false),
        Recorder::Probe(epoch) => (Some(epoch), false),
        Recorder::Unclaimed => (None, true),
    };
    let result = sqlx::query(RECORD_SQL.as_str())
        .bind(deployment)
        .bind(error.is_some())
        .bind(epoch)
        .bind(unclaimed)
        .execute(&store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?;
    store.caches.forget_health(deployment);
    if result.rows_affected() != 1 {
        return Err(InferenceError::ModelUnavailable);
    }
    Ok(())
}

/// Sanitized operator view: no endpoint, credential reference, model payload,
/// upstream response, or raw error text. Missing observations remain explicitly unknown.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct RouteHealth {
    pub deployment_id: Uuid,
    pub operator_disabled: bool,
    pub consecutive_failures: i32,
    pub open_until: Option<UtcInstant>,
    pub last_observed_at: Option<UtcInstant>,
    pub circuit_open: bool,
}

/// Caller must enforce live platform read authorization.
/// An expired circuit is eligible again, not declared healthy.
pub async fn health(store: &Store, deployment: Uuid) -> Result<RouteHealth, InferenceError> {
    sqlx::query_as::<_, RouteHealth>(
        r#"SELECT d.id AS deployment_id, NOT d.enabled AS operator_disabled,
                  COALESCE(h.consecutive_failures,0) AS consecutive_failures,
                  h.open_until, h.last_observed_at,
                  COALESCE(h.open_until > statement_timestamp(),false) AS circuit_open
           FROM deployments d
           LEFT JOIN deployment_routing r ON r.deployment_id=d.id
           LEFT JOIN deployment_health h ON h.deployment_id=d.id
           WHERE d.id=$1"#,
    )
    .bind(deployment)
    .fetch_optional(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?
    .ok_or(InferenceError::ModelUnavailable)
}
