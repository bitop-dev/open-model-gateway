//! Governance touch points of async jobs (`crate::jobs`). Admission and
//! settlement of a job attempt go through the ordinary `admit_*`/`finish`
//! functions, so budget totals (0015 triggers) stay correct; this module only
//! adds the acceptance of a job (its lease extension and record in one
//! transaction) and a read-only bound preview.
use super::*;
use gateway_datetime_compat::UtcInstant;

/// Record a job before its provider create (#662): extend the lease of its
/// still-pending reservation to `until` (never shortens) and run `insert`
/// (the job row, without a provider id yet), in one transaction, so the job
/// never exists without a reservation that lives as long as the job, and the
/// provider is never called for a job that is not recorded. Either statement
/// failing rolls both back; nothing has been sent then, so the caller settles
/// the attempt as a known zero.
///
/// Lock order: the reservation row first (the lease statement), then the
/// counter rows the job row's trigger changes (`insert`). The lease change
/// touches no totals or counter row (the triggers see a zero delta), and an
/// admission that already treated the lease as expired linearizes before the
/// extension.
pub(crate) async fn accept_job<T>(
    store: &Store,
    execution: Uuid,
    until: UtcInstant,
    insert: impl AsyncFnOnce(&mut sqlx::PgConnection) -> Result<T, InferenceError>,
) -> Result<T, InferenceError> {
    let _queued = gate(&store.lock_gates.settlement).await;
    let mut tx = crate::db::begin(&store.pool)
        .await
        .map_err(record_storage)?;
    let extended = sqlx::query("UPDATE governance_reservations SET lease_expires_at=greatest(lease_expires_at,$2) WHERE execution_id=$1 AND state='pending'")
        .bind(execution)
        .bind(until)
        .execute(&mut *tx)
        .await
        .map_err(record_storage)?
        .rows_affected();
    if extended != 1 {
        // Its admission lease ran out before the job could be recorded (and
        // reconciliation took it): the provider is not called for it.
        tracing::warn!(execution_id = %execution, "job reservation no longer pending before its record; nothing is sent");
        return Err(InferenceError::Storage);
    }
    let recorded = insert(&mut *tx).await?;
    tx.commit().await.map_err(record_storage)?;
    Ok(recorded)
}

/// A failed step of the record, before anything was sent: counted at
/// `job_storage`. A failed COMMIT acknowledgement may still have committed
/// the row; it then ends by the video create-grace rule.
fn record_storage(error: sqlx::Error) -> InferenceError {
    crate::db::accounting_failure::job_storage(error);
    InferenceError::Storage
}

/// Preview of the conservative hold a batch of `requests` lines with these
/// output maxima would take under the deployment's current price, without
/// reserving anything. `Ok(None)` when unpriced, out of the price's
/// ceilings or unbounded (admission would refuse it under any budget).
pub async fn batch_bound_preview(
    store: &Store,
    deployment: Uuid,
    requests: u32,
    output_tokens: u64,
    max_line_output: u32,
) -> Result<Option<i64>, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    let price = sqlx::query_as::<_, Price>(&format!("SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1"))
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    let Some(p) = price else {
        return Ok(None);
    };
    if requests == 0
        || max_line_output == 0
        || p.input_token_limit < 1
        || i64::from(max_line_output) > p.output_token_limit
    {
        return Ok(None);
    }
    let ceilings = MeterUsage {
        requests: Some(u64::from(requests)),
        ..MeterUsage::default()
    };
    match p.bound_scaled(u64::from(requests), output_tokens, &ceilings, false) {
        Ok(bound) => Ok(bound),
        Err(InferenceError::Configuration) => Ok(None),
        Err(e) => Err(e),
    }
}
