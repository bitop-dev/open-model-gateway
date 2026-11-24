//! Bounded background poller (`serve`, runtime role): every interval it
//! claims at most [`BATCH`] due, unsettled jobs one at a time, refreshes each
//! from its provider with a bounded deadline, applies the forward-only state
//! machine and settles terminal jobs idempotently. Errors back off per job;
//! jobs past their poll deadline are left to lease reconciliation (the hold
//! is retained as unknown).
//!
//! Each process polls as one owner. A claim is a lease on the job row with a
//! bumped epoch (`store::claim_due`, [`super::fence`]): another poller skips
//! the job while the lease runs, and once it takes the job over, every write
//! the earlier claim still makes to a native batch is refused. The lease is
//! renewed while the job is refreshed, concurrently with the refresh, so a
//! refresh can run for its whole deadline, and a crashed poller's jobs are
//! taken again after [`LEASE_SECONDS`]. A refused renewal marks the claim
//! lost: a native collection then stops at its next safe point and removes
//! the result files it created.
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use super::{
    fence::{BatchFence, Renewed},
    *,
};

/// Jobs claimed per tick.
pub const BATCH: i64 = 32;
/// Deadline of one job refresh (an output-file scan has its own deadline).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60 + 60);
/// Poller lease of a claimed job, renewed every [`RENEW`] while it is refreshed.
const LEASE_SECONDS: f64 = 60.0;
const RENEW: Duration = Duration::from_secs(15);

/// Lease term and renewal period of a poller claim.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PollLease {
    pub(crate) seconds: f64,
    pub(crate) renew: Duration,
}
impl PollLease {
    pub(crate) const DEFAULT: Self = Self {
        seconds: LEASE_SECONDS,
        renew: RENEW,
    };
    /// One renewal gets two thirds of the period (10 s of 15 s): a hung
    /// connection counts as a failed renewal before the next one is due.
    fn renew_timeout(self) -> Duration {
        self.renew * 2 / 3
    }
}

/// Start the poller when an interval is configured.
pub fn start(jobs: Jobs) -> Option<tokio::task::JoinHandle<()>> {
    let every = jobs.limits.poll_interval?;
    let owner = Uuid::new_v4();
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match jobs.poll_as(owner).await {
                Ok(n) if n > 0 => tracing::debug!(jobs = n, "async jobs polled"),
                Ok(_) => {}
                Err(_) => tracing::warn!("async job poll incomplete; retrying next interval"),
            }
        }
    }))
}

impl Jobs {
    /// The lease of a poller claim.
    #[cfg(not(test))]
    fn poll_lease(&self) -> PollLease {
        PollLease::DEFAULT
    }
    /// The lease of a poller claim: tests may shorten it.
    #[cfg(test)]
    fn poll_lease(&self) -> PollLease {
        self.poll_lease
    }

    /// One poll pass as an owner of its own; returns how many jobs were
    /// refreshed successfully.
    pub async fn poll_once(&self) -> Result<usize, InferenceError> {
        self.poll_as(Uuid::new_v4()).await
    }

    /// One poll pass as `owner`.
    pub(crate) async fn poll_as(&self, owner: Uuid) -> Result<usize, InferenceError> {
        let interval = self.poll_seconds();
        let lease = self.poll_lease();
        let mut done = 0;
        for _ in 0..BATCH {
            let Some((job, fence)) = store::claim_due(&self.store, owner, interval, lease.seconds)
                .await
                .map_err(|_| InferenceError::Storage)?
            else {
                break;
            };
            let lost = AtomicBool::new(false);
            let refreshed = tokio::time::timeout(REFRESH_TIMEOUT, self.poll_job(job, fence, &lost));
            let refreshed = self.renewing(fence, lease, &lost, refreshed).await;
            let failed = match refreshed {
                Ok(Ok(())) => {
                    done += 1;
                    None
                }
                _ => {
                    tracing::warn!(job_id = %fence.job, "async job refresh failed; backing off");
                    Some(interval)
                }
            };
            // A refused end is the normal case of a claim another poller took.
            let _ = fence.end_poll(&self.store, failed).await;
        }
        Ok(done)
    }

    /// Run `work` while renewing the claim's lease every `lease.renew`. The
    /// renewal runs beside the work in the same task, so a renewal waiting on
    /// the job row behind one of the work's fenced transactions never keeps
    /// that transaction from finishing. A refused renewal sets `lost` and the
    /// work stops at its next safe point; a renewal of a settled batch ends
    /// the renewals; the work always runs to its end.
    async fn renewing<T>(
        &self,
        fence: BatchFence,
        lease: PollLease,
        lost: &AtomicBool,
        work: impl Future<Output = T>,
    ) -> T {
        let mut work = std::pin::pin!(work);
        let renewal = std::pin::pin!(self.renew_claim(fence, lease, lost));
        tokio::select! {
            done = &mut work => done,
            () = renewal => work.await,
        }
    }

    /// Renew the lease until the claim ended (`lost`) or the batch settled.
    async fn renew_claim(&self, fence: BatchFence, lease: PollLease, lost: &AtomicBool) {
        let mut renew =
            tokio::time::interval_at(tokio::time::Instant::now() + lease.renew, lease.renew);
        loop {
            renew.tick().await;
            let renewed = fence.renew(&self.store, lease.seconds);
            match tokio::time::timeout(lease.renew_timeout(), renewed).await {
                Ok(Ok(Renewed::Held { .. })) => {}
                Ok(Ok(Renewed::Settled)) => return,
                Ok(Ok(Renewed::Fenced)) => {
                    lost.store(true, Ordering::SeqCst);
                    return;
                }
                _ => tracing::warn!(job_id = %fence.job, "async job lease renewal failed"),
            }
        }
    }

    async fn poll_job(&self, job: JobRow, fence: BatchFence, lost: &AtomicBool) -> JobResult<()> {
        match job.job_kind() {
            Some(JobKind::Video) => {
                self.refresh_video(job).await?;
            }
            Some(JobKind::Batch) => match job.mode() {
                Some(BatchMode::Native) => self.poll_native(job, fence, lost).await?,
                // Gateway-run batches belong to the batch runner.
                Some(BatchMode::Gateway) => {}
                None => {
                    self.refresh_legacy(job).await?;
                }
            },
            None => return Err(InferenceError::Storage.into()),
        }
        Ok(())
    }
}
