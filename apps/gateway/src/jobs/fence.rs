//! The claim fence of a gateway-run batch (migration 0081, #638).
//!
//! A runner owns a batch while `(runner_id, runner_epoch)` on the job row are
//! the pair it was given when it claimed the batch; the claim bumps the epoch,
//! so a later claim by any runner, including a new run of the same process,
//! ends the earlier run's ownership. Every runner write to the batch's rows
//! starts its transaction by locking the job row with that pair in the
//! predicate: `FOR KEY SHARE` when it writes other rows only, `FOR NO KEY
//! UPDATE` when it also changes the job row. A claim locks the row `FOR
//! UPDATE` (`SKIP LOCKED`), which conflicts with both, so it skips a batch
//! with a write in flight and takes it on a later tick; a write that starts
//! after a claim finds no row and is refused ([`FenceError::Stale`]). The
//! runner then writes nothing more to the batch. Every takeover must lock the
//! row `FOR UPDATE`, as the claim does: a plain `UPDATE` takes only `NO KEY
//! UPDATE`, which `KEY SHARE` does not exclude.
//!
//! Lock order: the job row is the first row lock of every runner transaction,
//! before the route lock, line rows, segment rows and, in governance, the
//! reservation and totals rows (docs/governance.md). Transactions that change
//! the job row take it `FOR NO KEY UPDATE` at the start, so no transaction
//! upgrades a lock it already holds; `KEY SHARE` and `NO KEY UPDATE` do not
//! conflict, so admission can proceed while a segment or progress update holds
//! the job row. Segment registrations serialize their sequence allocation.
//!
//! A frozen runner inside a fenced transaction would hold the row against
//! every claim: each fenced transaction therefore ends the session after
//! [`IDLE_IN_TRANSACTION`] without a statement, which releases the lock.
//!
//! The poller claims provider jobs (videos, native and 0016 batches) with the
//! same columns and the same rule (`store::claim_due`): the runner and the
//! poller never claim the same row, because the runner takes only
//! gateway-run batches and the poller every other job. Every poller write to
//! a native batch's row up to and including its finish goes through the
//! fenced methods below; what follows a committed finish (the idempotent
//! settlement, the deletion of the provider's copies) is not fenced
//! (`jobs::native`). Video and 0016 batch observations (`store::observe`)
//! carry their writer: a poller's write checks its owner and epoch, a client
//! read outside any claim (`get_video`) writes only while no poller holds the
//! job, and progress and counts only grow, so neither a stale poller nor a
//! client read writes older values; `state` stays compare-and-set.
use sqlx::{Postgres, Transaction, postgres::PgArguments, query::Query};
use uuid::Uuid;

use super::{
    store::{self, Finished, JobRow, finish_locked_batch, job_storage, lock_batch_for_finish},
    types::RequestCounts,
};
use crate::store::Store;

/// The error code of a native batch ended because it could not be submitted
/// in time ([`BatchFence::expire_submission`]).
pub(crate) const SUBMISSION_EXPIRED: &str = "submission_expired";

/// Time kept after a native batch's completion window, before its poll
/// deadline, to collect its results: a provider that uses its whole window
/// still finishes while the batch is polled.
pub(crate) const COLLECTION_MARGIN_HOURS: i32 = 1;

/// A native batch can still be submitted in time (its row as `async_jobs`):
/// its full completion window and [`COLLECTION_MARGIN_HOURS`] are left before
/// its poll deadline, by the database clock. `claim_submit` requires it and
/// `expire_submission` its negation, so the two never both hold or both fail.
pub(crate) static SUBMIT_IN_TIME: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "(clock_timestamp()+make_interval(hours=>coalesce(async_jobs.completion_window_hours,24)::int+{COLLECTION_MARGIN_HOURS})<=async_jobs.poll_deadline_at)"
    )
});

/// What a runner holds for one claim of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BatchFence {
    pub(crate) job: Uuid,
    pub(crate) runner: Uuid,
    pub(crate) epoch: i64,
}

/// A recovered claim cancelled before dispatch, with the preceding provider failure.
pub(crate) struct UndispatchedRetry {
    pub execution: Uuid,
    pub attempt: i16,
    pub status: u16,
    pub code: &'static str,
}

/// A fenced write that did not happen.
#[derive(Debug)]
pub(crate) enum FenceError {
    /// The claim ended: the batch belongs to another claim now.
    Stale,
    /// A write failed and was not applied. Counted where it ran.
    Storage,
    /// Result file locked by a concurrent operation. The batch can retry.
    FileBusy,
    /// COMMIT did not return a successful acknowledgement. Counted where it ran.
    Unknown,
}
impl From<sqlx::Error> for FenceError {
    fn from(_: sqlx::Error) -> Self {
        Self::Storage
    }
}
/// A rebuild leaves both candidate files unlinked.
pub(super) enum Publication {
    Linked(Box<JobRow>),
    RebuildCancelled,
    RebuildExpired,
}
/// What a lease renewal found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Renewed {
    /// The claim is current; whether a cancel was requested.
    Held { cancel: bool },
    /// The batch is settled: nothing is left to renew.
    Settled,
    /// The claim ended.
    Fenced,
}

pub(crate) type FenceResult<T> = Result<T, FenceError>;

/// The longest a fenced transaction may sit between statements.
pub(crate) const IDLE_IN_TRANSACTION: &str = "30s";

/// How a transaction holds the job row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    /// Writes other rows only (`FOR KEY SHARE`).
    Rows,
    /// Also changes the job row.
    Update,
}

/// Lines represented by one encrypted output file.
#[derive(Default)]
pub(crate) struct SegmentLines<'a> {
    pub(crate) succeeded: &'a [(i32, Uuid)],
    pub(crate) failed: &'a [(i32, Uuid)],
}

impl BatchFence {
    /// Whether the claim is still current, holding the job row until `tx` ends
    /// when it is. The first statement of every runner write transaction.
    /// Database errors are not counted here: the caller maps them.
    pub(crate) async fn hold(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hold: Hold,
    ) -> Result<bool, sqlx::Error> {
        // `set_config` is evaluated for a row that matches and is local to
        // the transaction; a refused hold (no row) ends the transaction.
        let sql = match hold {
            Hold::Rows => {
                "SELECT set_config('idle_in_transaction_session_timeout',$4,true) FROM async_jobs WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 FOR KEY SHARE"
            }
            Hold::Update => {
                "SELECT set_config('idle_in_transaction_session_timeout',$4,true) FROM async_jobs WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 FOR NO KEY UPDATE"
            }
        };
        Ok(sqlx::query_scalar::<_, String>(sql)
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
            .bind(IDLE_IN_TRANSACTION)
            .fetch_optional(&mut **tx)
            .await?
            .is_some())
    }

    /// [`Self::hold`] for the job module: a failed statement is counted once.
    pub(crate) async fn hold_counted(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hold: Hold,
    ) -> FenceResult<()> {
        if self.hold(tx, hold).await.map_err(job_storage)? {
            Ok(())
        } else {
            Err(FenceError::Stale)
        }
    }

    pub(super) async fn begin(
        &self,
        store: &Store,
        hold: Hold,
    ) -> FenceResult<Transaction<'static, Postgres>> {
        let mut tx = crate::db::begin(&store.pool).await.map_err(job_storage)?;
        self.hold_counted(&mut tx, hold).await?;
        Ok(tx)
    }

    /// A statement whose first three parameters are the fence's.
    fn query<'q>(&self, sql: &'q str) -> Query<'q, Postgres, PgArguments> {
        sqlx::query(sql)
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
    }

    /// One statement on the job row alone: the predicate is the fence (the
    /// update re-checks it on the locked row), so no transaction is needed.
    async fn update(
        &self,
        store: &Store,
        query: Query<'_, Postgres, PgArguments>,
    ) -> FenceResult<()> {
        let done = query
            .execute(&store.pool)
            .await
            .map_err(job_storage)?
            .rows_affected();
        if done == 0 {
            return Err(FenceError::Stale);
        }
        Ok(())
    }

    /// Renew the lease.
    pub(crate) async fn renew(
        &self,
        store: &Store,
        lease_seconds: f64,
    ) -> Result<Renewed, sqlx::Error> {
        let row: Option<(bool, bool)> = sqlx::query_as("UPDATE async_jobs SET runner_lease_until=clock_timestamp()+make_interval(secs=>$4) WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND runner_lease_until IS NOT NULL RETURNING cancel_requested_at IS NOT NULL,settled_at IS NOT NULL")
            .bind(self.job).bind(self.runner).bind(self.epoch).bind(lease_seconds)
            .fetch_optional(&store.pool).await.map_err(job_storage)?;
        Ok(match row {
            None => Renewed::Fenced,
            Some((_, true)) => Renewed::Settled,
            Some((cancel, false)) => Renewed::Held { cancel },
        })
    }

    /// Give the batch up at once (a failed run), so another runner need not
    /// wait for the lease term.
    pub(crate) async fn release(&self, store: &Store) -> FenceResult<()> {
        self.update(
            store,
            self.query("UPDATE async_jobs SET runner_lease_until=NULL WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3"),
        )
        .await
    }

    pub(crate) async fn mark_running(&self, store: &Store) -> FenceResult<()> {
        // No row is also the normal case of a resumed batch that is running
        // already; the first fenced write after this finds a lost claim.
        let query = self.query("UPDATE async_jobs SET state='in_progress',upstream_status=CASE WHEN cancel_requested_at IS NULL THEN 'in_progress' ELSE upstream_status END,in_progress_at=coalesce(in_progress_at,clock_timestamp()),last_progress_at=coalesce(last_progress_at,clock_timestamp()) WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND state='queued'");
        match self.update(store, query).await {
            Ok(_) | Err(FenceError::Stale) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Display status of an unfinished batch.
    pub(crate) async fn set_status(&self, store: &Store, status: &str) -> FenceResult<()> {
        let query = self.query("UPDATE async_jobs SET upstream_status=CASE WHEN cancel_requested_at IS NULL THEN $4 ELSE upstream_status END,finalizing_at=CASE WHEN $4='finalizing' THEN coalesce(finalizing_at,clock_timestamp()) ELSE finalizing_at END WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND state IN('queued','in_progress')");
        self.update(store, query.bind(status)).await
    }

    /// Lines finished since the last call.
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) async fn progress(
        &self,
        store: &Store,
        completed: i32,
        failed: i32,
    ) -> FenceResult<()> {
        let query = self.query("UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+$4,request_failed=coalesce(request_failed,0)+$5,last_progress_at=clock_timestamp() WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3");
        self.update(store, query.bind(completed).bind(failed)).await
    }

    /// Record the terminal state and result files. [`FenceError::Stale`] when
    /// the claim ended: nothing was recorded.
    pub(super) async fn finish_batch(
        &self,
        store: &Store,
        f: &Finished<'_>,
    ) -> FenceResult<Publication> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let job = lock_batch_for_finish(&mut tx, self.job).await?;
        let mut result = Finished { ..*f };
        if !job.job_state().is_terminal() {
            let unavailable = f.error_code == Some("hold_unavailable");
            // Budget refusal retains its own disposition. Cancellation precedes
            // expiry, and either takes precedence over an unavailable envelope.
            if job.cancel_requested_at.is_some()
                && (matches!(
                    f.state,
                    super::JobState::Completed | super::JobState::Expired
                ) || unavailable)
            {
                if f.error_file.is_some() {
                    return Ok(Publication::RebuildCancelled);
                }
                result.state = super::JobState::Cancelled;
                result.error_code = None;
            } else if unavailable
                && gateway_datetime_compat::UtcInstant::now()
                    >= job.created_at + job.completion_window()
            {
                return Ok(Publication::RebuildExpired);
            }
        }
        let row = finish_locked_batch(&mut tx, job, &result)
            .await
            .map_err(finish_error)?;
        // An error at COMMIT cannot authorize cleanup. The sweeper can
        // check whether the job links actually committed.
        tx.commit().await.map_err(|e| {
            job_storage(e);
            FenceError::Unknown
        })?;
        Ok(Publication::Linked(Box::new(row)))
    }

    pub(crate) async fn mark_settled(&self, store: &Store) -> FenceResult<()> {
        // Nothing to do when it is settled already; a lost claim settles
        // nothing, and the claim that holds the batch settles it.
        let query = self.query("UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND settled_at IS NULL AND state IN('completed','failed','cancelled','expired')");
        query.execute(&store.pool).await.map_err(job_storage)?;
        Ok(())
    }

    /// Claim a line's first attempt. Inserting the row is the claim, so a line
    /// can never run twice (even across runners).
    pub(crate) async fn claim_line(
        &self,
        store: &Store,
        workspace: Uuid,
        line_no: i32,
        execution: Uuid,
    ) -> FenceResult<bool> {
        let mut tx = self.begin(store, Hold::Rows).await?;
        let inserted = sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id) VALUES($1,$2,$3,'running',$4) ON CONFLICT DO NOTHING")
            .bind(self.job).bind(workspace).bind(line_no).bind(execution)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        tx.commit().await.map_err(job_storage)?;
        Ok(inserted)
    }

    /// Record a failed attempt. With `count` the batch's counters move in the
    /// same transaction, and only when this call is what moved the line out of
    /// `running` (a line already interrupted or finished is not counted
    /// twice). Returns whether the line changed.
    pub(crate) async fn fail_line(
        &self,
        store: &Store,
        line_no: i32,
        execution: Uuid,
        status_code: Option<u16>,
        error_code: Option<&str>,
        count: bool,
    ) -> FenceResult<bool> {
        let mut tx = self
            .begin(store, if count { Hold::Update } else { Hold::Rows })
            .await?;
        let changed = sqlx::query("UPDATE batch_lines SET state='failed',status_code=$4,error_code=$5,finished_at=clock_timestamp() WHERE job_id=$1 AND line_no=$2 AND execution_id=$3 AND state='running' AND NOT awaiting_segment")
            .bind(self.job).bind(line_no).bind(execution)
            .bind(status_code.map(|s| s as i16)).bind(error_code)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        if count && changed {
            count_lines(&mut tx, self.job, 0, 1).await?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(changed)
    }

    /// Count the known provider failure under its execution ID. A committed
    /// but undispatched retry retains its claim count, while the result
    /// belongs to the earlier execution that actually failed.
    pub(crate) async fn count_retry_failure(
        &self,
        store: &Store,
        line: i32,
        execution: Uuid,
        recovered: Option<UndispatchedRetry>,
    ) -> FenceResult<bool> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let failed = if let Some(retry) = recovered {
            sqlx::query(
                "UPDATE batch_lines SET state='failed', execution_id=$5, status_code=$6,
                        error_code=$7, finished_at=clock_timestamp()
                 WHERE job_id=$1 AND line_no=$2 AND execution_id=$3 AND attempts=$4
                   AND state='running' AND segment IS NULL AND NOT awaiting_segment",
            )
            .bind(self.job)
            .bind(line)
            .bind(retry.execution)
            .bind(retry.attempt)
            .bind(execution)
            .bind(retry.status as i16)
            .bind(retry.code)
            .execute(&mut *tx)
            .await
            .map_err(job_storage)?
            .rows_affected()
                == 1
        } else {
            sqlx::query_scalar::<_, i32>("SELECT line_no FROM batch_lines WHERE job_id=$1 AND line_no=$2 AND execution_id=$3 AND state='failed' AND segment IS NULL FOR UPDATE")
                .bind(self.job).bind(line).bind(execution)
                .fetch_optional(&mut *tx).await.map_err(job_storage)?.is_some()
        };
        if failed {
            count_lines(&mut tx, self.job, 0, 1).await?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(failed)
    }

    /// Release the route once the provider attempt has settled. Recovery
    /// still treats the line as running until its result segment commits.
    pub(crate) async fn await_segment(
        &self,
        store: &Store,
        line: i32,
        execution: Uuid,
    ) -> FenceResult<bool> {
        let mut tx = self.begin(store, Hold::Rows).await?;
        let changed = sqlx::query("UPDATE batch_lines SET awaiting_segment=true WHERE job_id=$1 AND line_no=$2 AND execution_id=$3 AND state='running' AND NOT awaiting_segment")
            .bind(self.job).bind(line).bind(execution)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected();
        tx.commit().await.map_err(job_storage)?;
        Ok(changed == 1)
    }

    /// A line a crashed or lost runner left `running` is `interrupted`: never
    /// executed again. Counted failed in the same transaction. Returns how
    /// many.
    pub(crate) async fn interrupt_running(&self, store: &Store) -> FenceResult<i32> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let n = sqlx::query("UPDATE batch_lines SET state='interrupted',awaiting_segment=false,error_code='interrupted',finished_at=clock_timestamp() WHERE job_id=$1 AND state='running'")
            .bind(self.job).execute(&mut *tx).await.map_err(job_storage)?.rows_affected();
        let n = i32::try_from(n).unwrap_or(i32::MAX);
        if n > 0 {
            count_lines(&mut tx, self.job, 0, n).await?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(n)
    }

    /// Record a stored result segment and the lines it holds, as the next
    /// segment number. The number is taken inside the fenced transaction, so
    /// one failed or lost insert never makes a later one collide.
    pub(crate) async fn insert_segment(
        &self,
        store: &Store,
        workspace: Uuid,
        file: Uuid,
        lines: SegmentLines<'_>,
    ) -> FenceResult<i32> {
        // A process that predates the epoch allocates its own numbers and can
        // take ours between the read and the insert: take the next one.
        let length = (lines.succeeded.len() + lines.failed.len()) as i32;
        let (line_numbers, executions): (Vec<_>, Vec<_>) = lines.succeeded.iter().copied().unzip();
        let (failed_lines, failed_executions): (Vec<_>, Vec<_>) =
            lines.failed.iter().copied().unzip();
        let mut collisions = 0;
        loop {
            // Serialize registrations with other progress updates.
            let mut tx = self.begin(store, Hold::Update).await?;
            let seq: Result<i32, sqlx::Error> = sqlx::query_scalar("INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) SELECT $1,$2,coalesce(max(seq)+1,0),$3,$4 FROM batch_segments WHERE job_id=$1 RETURNING seq")
                .bind(self.job).bind(workspace).bind(file).bind(length)
                .fetch_one(&mut *tx).await;
            let seq = match seq {
                Ok(seq) => seq,
                Err(e) if collisions < 3 && is_unique_violation(&e) => {
                    collisions += 1;
                    continue;
                }
                Err(e) => return Err(job_storage(e).into()),
            };
            if !line_numbers.is_empty() {
                let changed = sqlx::query("UPDATE batch_lines l SET segment=$2,state='succeeded',awaiting_segment=false,finished_at=clock_timestamp(),status_code=200 FROM unnest($3::integer[],$4::uuid[]) AS ready(line_no,execution_id) WHERE l.job_id=$1 AND l.line_no=ready.line_no AND l.execution_id=ready.execution_id AND l.state='running' AND l.awaiting_segment AND l.segment IS NULL")
                    .bind(self.job).bind(seq).bind(&line_numbers).bind(&executions)
                    .execute(&mut *tx).await.map_err(job_storage)?.rows_affected();
                if changed != line_numbers.len() as u64 {
                    // No part of a segment can publish if an attempt is no longer current.
                    return Err(FenceError::Storage);
                }
                count_lines(&mut tx, self.job, changed as i32, 0).await?;
            }
            if !failed_lines.is_empty() {
                let changed = sqlx::query("UPDATE batch_lines l SET segment=$2 FROM unnest($3::integer[],$4::uuid[]) AS ready(line_no,execution_id) WHERE l.job_id=$1 AND l.line_no=ready.line_no AND l.execution_id=ready.execution_id AND l.state='failed' AND l.segment IS NULL")
                    .bind(self.job).bind(seq).bind(&failed_lines).bind(&failed_executions)
                    .execute(&mut *tx).await.map_err(job_storage)?.rows_affected();
                if changed != failed_lines.len() as u64 {
                    return Err(FenceError::Storage);
                }
            }
            // Once COMMIT is sent, keep the file on any missing acknowledgement.
            // Retention can reclaim an orphan; a committed segment still needs its file.
            tx.commit().await.map_err(|e| {
                job_storage(e);
                FenceError::Unknown
            })?;
            return Ok(seq);
        }
    }
}

/// Poller writes to a native batch. A refused hold ([`FenceError::Stale`])
/// means another poller holds the batch: nothing was written.
impl BatchFence {
    /// Claim the native batch's one submission: exclusive with an early
    /// cancel, and refused once the claim ended, so a poller that lost the
    /// batch never submits it. Refused while its route is drained
    /// (`routing::drain`): the job stays queued and is polled again at its
    /// next poll; a submission claimed before the drain committed is in
    /// flight and proceeds. Refused too unless the batch's full completion
    /// window and [`COLLECTION_MARGIN_HOURS`] are left before its poll
    /// deadline ([`SUBMIT_IN_TIME`]): otherwise its results could not be
    /// collected, and [`Self::expire_submission`] ends it instead.
    pub(crate) async fn claim_submit(&self, store: &Store) -> FenceResult<bool> {
        let sql = format!(
            "UPDATE async_jobs SET submit_started_at=clock_timestamp() WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND submit_started_at IS NULL AND upstream_id IS NULL AND cancel_requested_at IS NULL AND state='queued' AND {} AND NOT EXISTS(SELECT 1 FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=async_jobs.deployment_id AND {})",
            *SUBMIT_IN_TIME,
            crate::routing::drain::ROUTE_DRAINED
        );
        let claimed = self
            .query(&sql)
            .execute(&store.pool)
            .await
            .map_err(job_storage)?
            .rows_affected();
        Ok(claimed == 1)
    }

    /// Display status of an unfinished native batch, with the provider's
    /// counts when it reported them.
    pub(crate) async fn native_status(
        &self,
        store: &Store,
        counts: Option<RequestCounts>,
        status: &str,
    ) -> FenceResult<()> {
        let mut tx = self.begin(store, Hold::Update).await?;
        if let Some(counts) = counts {
            store::observe_counts(&mut tx, self.job, counts).await?;
        }
        store::observe_status(&mut tx, self.job, status).await?;
        tx.commit().await.map_err(job_storage)?;
        Ok(())
    }

    /// One provider observation of an unfinished native batch: its counts,
    /// whether it runs, its display status and the next poll, together.
    pub(crate) async fn observe_native(
        &self,
        store: &Store,
        counts: Option<RequestCounts>,
        running: bool,
        status: &str,
        interval_seconds: f64,
    ) -> FenceResult<()> {
        let mut tx = self.begin(store, Hold::Update).await?;
        if let Some(counts) = counts {
            store::observe_counts(&mut tx, self.job, counts).await?;
        }
        if running {
            store::mark_running(&mut tx, self.job).await?;
        }
        store::observe_status(&mut tx, self.job, status).await?;
        store::poll_ok(&mut tx, self.job, interval_seconds).await?;
        tx.commit().await.map_err(job_storage)?;
        Ok(())
    }

    /// Record a native batch's terminal state and result files. A batch that
    /// is terminal already keeps what it recorded; the row says which files
    /// it links, and the flag whether this call recorded the finish (so a
    /// finish is counted once, by the claim that made it).
    pub(crate) async fn finish_native(
        &self,
        store: &Store,
        f: &Finished<'_>,
    ) -> FenceResult<(JobRow, bool)> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let locked = lock_batch_for_finish(&mut tx, self.job)
            .await
            .map_err(finish_error)?;
        let recorded = matches!(locked.state.as_str(), "queued" | "in_progress");
        let row = finish_locked_batch(&mut tx, locked, f)
            .await
            .map_err(finish_error)?;
        // An error at COMMIT cannot authorize cleanup (as for a runner).
        tx.commit().await.map_err(|e| {
            job_storage(e);
            FenceError::Unknown
        })?;
        Ok((row, recorded))
    }

    /// End a native batch that was never claimed for submission and can no
    /// longer be submitted in time (not [`SUBMIT_IN_TIME`]: its route was
    /// drained, or could not be reached, until then): `failed` with
    /// `submission_expired`. Nothing was sent; the caller settles it at zero.
    /// `None` when it is not such a batch (claimed for submission, submitted,
    /// cancelled, finished, or still in time).
    pub(crate) async fn expire_submission(&self, store: &Store) -> FenceResult<Option<JobRow>> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let locked = lock_batch_for_finish(&mut tx, self.job)
            .await
            .map_err(finish_error)?;
        let late: bool = sqlx::query_scalar(&format!(
            "SELECT NOT ({}) FROM async_jobs WHERE id=$1",
            *SUBMIT_IN_TIME
        ))
        .bind(self.job)
        .fetch_one(&mut *tx)
        .await
        .map_err(job_storage)?;
        if !late
            || locked.upstream_id.is_some()
            || locked.submit_started_at.is_some()
            || locked.cancel_requested_at.is_some()
            || locked.state != "queued"
        {
            tx.commit().await.map_err(job_storage)?;
            return Ok(None);
        }
        let row = finish_locked_batch(
            &mut tx,
            locked,
            &Finished {
                state: super::JobState::Failed,
                error_code: Some(SUBMISSION_EXPIRED),
                output_file: None,
                error_file: None,
                counts: None,
            },
        )
        .await
        .map_err(finish_error)?;
        tx.commit().await.map_err(|e| {
            job_storage(e);
            FenceError::Unknown
        })?;
        Ok(Some(row))
    }

    /// End a started submission that never recorded its provider id as
    /// `f` (interrupted), only if the id is still absent under the finish
    /// lock: an id that a slow submitter recorded after this claim read the
    /// batch means the batch is submitted and is polled, not interrupted.
    /// `None` when nothing was recorded.
    pub(crate) async fn interrupt_submission(
        &self,
        store: &Store,
        f: &Finished<'_>,
    ) -> FenceResult<Option<JobRow>> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let locked = lock_batch_for_finish(&mut tx, self.job)
            .await
            .map_err(finish_error)?;
        if locked.upstream_id.is_some()
            || !matches!(locked.state.as_str(), "queued" | "in_progress")
        {
            tx.commit().await.map_err(job_storage)?;
            return Ok(None);
        }
        let row = finish_locked_batch(&mut tx, locked, f)
            .await
            .map_err(finish_error)?;
        tx.commit().await.map_err(|e| {
            job_storage(e);
            FenceError::Unknown
        })?;
        Ok(Some(row))
    }

    /// End the poller's claim; after a failed refresh also back off
    /// (interval × 2^failures, at most 32×). Refused once the claim ended:
    /// the next poll belongs to the claim that holds the job.
    pub(crate) async fn end_poll(&self, store: &Store, failed: Option<f64>) -> FenceResult<()> {
        let query = match failed {
            None => self.query("UPDATE async_jobs SET runner_lease_until=NULL WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3"),
            Some(interval) => self.query("UPDATE async_jobs SET runner_lease_until=NULL,poll_failures=poll_failures+1,last_polled_at=clock_timestamp(),next_poll_at=clock_timestamp()+make_interval(secs=>$4*power(2,least(poll_failures,5))) WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3").bind(interval),
        };
        self.update(store, query).await
    }
}

/// A lock wait on a result file (`NOWAIT`) is [`FenceError::FileBusy`].
fn finish_error(error: sqlx::Error) -> FenceError {
    match &error {
        sqlx::Error::Database(e) if e.code().as_deref() == Some("55P03") => FenceError::FileBusy,
        _ => FenceError::Storage,
    }
}

/// Add finished lines to the batch's counters (the job row is held).
async fn count_lines(
    tx: &mut Transaction<'_, Postgres>,
    job: Uuid,
    completed: i32,
    failed: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+$2,request_failed=coalesce(request_failed,0)+$3,last_progress_at=clock_timestamp() WHERE id=$1")
        .bind(job).bind(completed).bind(failed)
        .execute(&mut **tx).await.map_err(job_storage)?;
    Ok(())
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("23505"))
}
