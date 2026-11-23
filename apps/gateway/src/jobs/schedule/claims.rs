//! The claims and demand rows a gateway-run batch's runner writes (0022):
//! the exactly-once line claim under the route lock, explicit retries and the
//! demand heartbeat. Each is a fenced write ([`crate::jobs::fence`]).
use uuid::Uuid;

use super::{Pause, Scheduler, WaitRow, order_sql};
use crate::jobs::{
    fence::{BatchFence, FenceResult, Hold},
    store::job_storage,
};

/// Outcome of a claim attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Claimed,
    /// The line was already claimed (never run it again).
    Taken,
    /// The route runs its maximum.
    Full,
    /// Another batch's turn.
    Wait,
}

/// Result of trying to start a previously failed batch line again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetryClaim {
    Started,
    Cancelled,
    Full,
    LineChanged,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum RetryCommitFault {
    Rollback,
    LostAck,
}

impl Scheduler {
    /// Claim line `line` of the fenced batch on `deployment` when the route has room and
    /// it is this batch's turn. Serialized per route by a transaction lock;
    /// inserting the line row is the (exactly-once) claim.
    pub(crate) async fn claim(
        &self,
        fence: &BatchFence,
        workspace: Uuid,
        line: i32,
        execution: Uuid,
        deployment: Uuid,
        max_concurrency: i32,
    ) -> FenceResult<Claim> {
        let mut tx = crate::db::begin(&self.store.pool)
            .await
            .map_err(job_storage)?;
        fence.hold_counted(&mut tx, Hold::Rows).await?;
        if route_full(&mut tx, deployment, max_concurrency).await? {
            tx.commit().await.map_err(job_storage)?;
            return Ok(Claim::Full);
        }
        sqlx::query("INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,ready) VALUES($1,$2,$3,1,true) ON CONFLICT(deployment_id,job_id) DO UPDATE SET ready=true,updated_at=clock_timestamp()")
            .bind(fence.job)
            .bind(workspace)
            .bind(deployment)
            .execute(&mut *tx)
            .await.map_err(job_storage)?;
        let first: Option<Uuid> = sqlx::query_scalar(&order_sql(true))
            .bind(deployment)
            .fetch_optional(&mut *tx)
            .await
            .map_err(job_storage)?;
        if first != Some(fence.job) {
            tx.commit().await.map_err(job_storage)?;
            return Ok(Claim::Wait);
        }
        let inserted = sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id,deployment_id) VALUES($1,$2,$3,'running',$4,$5) ON CONFLICT DO NOTHING")
            .bind(fence.job)
            .bind(workspace)
            .bind(line)
            .bind(execution)
            .bind(deployment)
            .execute(&mut *tx)
            .await.map_err(job_storage)?
            .rows_affected();
        if inserted != 1 {
            tx.commit().await.map_err(job_storage)?;
            return Ok(Claim::Taken);
        }
        sqlx::query("UPDATE batch_route_waits SET last_claim_at=clock_timestamp() WHERE deployment_id=$1 AND job_id=$2")
            .bind(deployment)
            .bind(fence.job)
            .execute(&mut *tx)
            .await.map_err(job_storage)?;
        tx.commit().await.map_err(job_storage)?;
        Ok(Claim::Claimed)
    }

    /// An explicit retry of a failed line within the pinned route's concurrency, if routed.
    pub(crate) async fn claim_retry(
        &self,
        fence: &BatchFence,
        line: i32,
        attempts: i16,
        execution: Uuid,
        route: Option<(Uuid, i32)>,
    ) -> FenceResult<RetryClaim> {
        // Cancellation updates this same row. Hold its write lock from the
        // persisted decision through commit, including for an unrouted retry.
        let mut tx = fence.begin(&self.store, Hold::Update).await?;
        let cancelled: bool = sqlx::query_scalar(
            "SELECT cancel_requested_at IS NOT NULL FROM async_jobs WHERE id=$1",
        )
        .bind(fence.job)
        .fetch_one(&mut *tx)
        .await
        .map_err(job_storage)?;
        if cancelled {
            tx.commit().await.map_err(job_storage)?;
            return Ok(RetryClaim::Cancelled);
        }
        if let Some((deployment, max_concurrency)) = route
            && route_full(&mut tx, deployment, max_concurrency).await?
        {
            tx.commit().await.map_err(job_storage)?;
            return Ok(RetryClaim::Full);
        }
        let claimed = sqlx::query("UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=$4,status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=$1 AND line_no=$2 AND state='failed' AND attempts=$3 AND segment IS NULL")
            .bind(fence.job).bind(line).bind(attempts).bind(execution).execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        if !claimed {
            tx.commit().await.map_err(job_storage)?;
            return Ok(RetryClaim::LineChanged);
        }
        match self.commit_retry(tx).await.map_err(job_storage) {
            Ok(()) => Ok(RetryClaim::Started),
            Err(_) => self.recover_retry(fence, line, attempts, execution).await,
        }
    }

    /// The provider has not been called for this execution yet. Taking the
    /// same job lock waits out the original transaction and verifies the
    /// runner epoch. A committed retry already occupies its route slot.
    async fn recover_retry(
        &self,
        fence: &BatchFence,
        line: i32,
        attempts: i16,
        execution: Uuid,
    ) -> FenceResult<RetryClaim> {
        let mut tx = fence.begin(&self.store, Hold::Update).await?;
        let cancelled: Option<bool> = sqlx::query_scalar(
            "SELECT j.cancel_requested_at IS NOT NULL FROM batch_lines l
             JOIN async_jobs j ON j.id=l.job_id
             WHERE l.job_id=$1 AND l.line_no=$2 AND l.execution_id=$3 AND l.attempts=$4
               AND l.state='running' AND l.segment IS NULL AND NOT l.awaiting_segment",
        )
        .bind(fence.job)
        .bind(line)
        .bind(execution)
        .bind(attempts + 1)
        .fetch_optional(&mut *tx)
        .await
        .map_err(job_storage)?;
        tx.commit().await.map_err(job_storage)?;
        Ok(match cancelled {
            Some(false) => RetryClaim::Started,
            Some(true) => RetryClaim::Cancelled,
            None => RetryClaim::LineChanged,
        })
    }

    async fn commit_retry(
        &self,
        tx: sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), sqlx::Error> {
        #[cfg(test)]
        {
            let fault = self.retry_commit_fault.lock().unwrap().take();
            if let Some(fault) = fault {
                match fault {
                    RetryCommitFault::Rollback => tx.rollback().await?,
                    RetryCommitFault::LostAck => tx.commit().await?,
                }
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset).into());
            }
        }
        tx.commit().await
    }

    /// Replace this batch's demand rows (heartbeat). `legit` records that
    /// the batch is legitimately waiting (stall alerts pause their clock).
    pub(crate) async fn publish_waits(
        &self,
        fence: &BatchFence,
        workspace: Uuid,
        rows: &[WaitRow],
        legit: bool,
    ) -> FenceResult<()> {
        let mut tx = crate::db::begin(&self.store.pool)
            .await
            .map_err(job_storage)?;
        let hold = if legit { Hold::Update } else { Hold::Rows };
        fence.hold_counted(&mut tx, hold).await?;
        let ids: Vec<Uuid> = rows.iter().map(|r| r.deployment).collect();
        let lines: Vec<i32> = rows
            .iter()
            .map(|r| r.waiting_lines.clamp(0, 50_000))
            .collect();
        let reasons: Vec<Option<&str>> = rows.iter().map(|r| r.reason.map(Pause::as_str)).collect();
        let ready: Vec<bool> = rows.iter().map(|r| r.ready).collect();
        sqlx::query("DELETE FROM batch_route_waits WHERE job_id=$1 AND deployment_id<>ALL($2)")
            .bind(fence.job)
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(job_storage)?;
        sqlx::query("INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,reason,ready) SELECT $1,$2,d,n,r,y FROM unnest($3::uuid[],$4::integer[],$5::text[],$6::boolean[]) AS u(d,n,r,y) ON CONFLICT(deployment_id,job_id) DO UPDATE SET waiting_lines=excluded.waiting_lines,reason=excluded.reason,ready=excluded.ready,updated_at=clock_timestamp()")
            .bind(fence.job)
            .bind(workspace)
            .bind(&ids)
            .bind(&lines)
            .bind(&reasons)
            .bind(&ready)
            .execute(&mut *tx)
            .await.map_err(job_storage)?;
        if legit {
            sqlx::query("UPDATE async_jobs SET last_waited_at=clock_timestamp() WHERE id=$1 AND state IN('queued','in_progress')")
                .bind(fence.job)
                .execute(&mut *tx)
                .await.map_err(job_storage)?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(())
    }

    /// Remove a finished batch's demand.
    pub(crate) async fn clear_waits(&self, fence: &BatchFence) -> FenceResult<()> {
        let mut tx = crate::db::begin(&self.store.pool)
            .await
            .map_err(job_storage)?;
        fence.hold_counted(&mut tx, Hold::Rows).await?;
        sqlx::query("DELETE FROM batch_route_waits WHERE job_id=$1")
            .bind(fence.job)
            .execute(&mut *tx)
            .await
            .map_err(job_storage)?;
        tx.commit().await.map_err(job_storage)?;
        Ok(())
    }
}

async fn route_full(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deployment: Uuid,
    max_concurrency: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('omg_batch_route:'||$1::text,0))")
        .bind(deployment)
        .execute(&mut **tx)
        .await
        .map_err(job_storage)?;
    let running: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running' AND NOT awaiting_segment",
    )
    .bind(deployment)
    .fetch_one(&mut **tx)
    .await
    .map_err(job_storage)?;
    Ok(running >= i64::from(max_concurrency))
}
