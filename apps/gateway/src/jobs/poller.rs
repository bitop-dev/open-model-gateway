//! Bounded background poller (`serve`, runtime role): every interval it
//! claims due, unsettled jobs, refreshes each from its provider with a
//! bounded deadline, applies the forward-only state machine and settles
//! terminal jobs idempotently. Errors back off per job; jobs past their poll
//! deadline are left to lease reconciliation (the hold is retained as
//! unknown).
//!
//! A process refreshes at most `GATEWAY_JOB_POLL_CONCURRENCY` jobs at once
//! (`JobLimits::poll_concurrency`). The refreshes run concurrently in the
//! poller's own task, across ticks: each tick claims only as many jobs as
//! there are free slots, in one statement, so one refresh that runs for its
//! whole deadline (a large native collection) holds one slot while the other
//! slots keep refreshing new submissions, status and cancels. Nothing is
//! spawned: dropping the poller drops every refresh in flight, which cancels
//! its upstream request.
//!
//! Each process polls as one owner. A claim is a lease on the job row with a
//! bumped epoch (`store::claim_due_many`, [`super::fence`]): another poller
//! skips the job while the lease runs, and once it takes the job over, every
//! write the earlier claim still makes to a native batch is refused. The
//! lease is renewed while the job is refreshed, concurrently with the
//! refresh, so a refresh can run for its whole deadline, and a crashed
//! poller's jobs are taken again after [`LEASE_SECONDS`]. A refused renewal
//! marks the claim lost: a native collection then stops at its next safe
//! point and removes the result files it created.
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};

use super::{
    fence::{BatchFence, Renewed},
    *,
};

/// Jobs one [`Jobs::poll_once`] pass claims at most.
pub const BATCH: usize = 32;
/// Deadline of one job refresh (an output-file scan has its own deadline).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60 + 60);
/// Poller lease of a claimed job, renewed every [`RENEW`] while it is refreshed.
const LEASE_SECONDS: f64 = 60.0;
const RENEW: Duration = Duration::from_secs(15);

/// The outcome of one lease renewal, a fixed metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Renewal {
    /// The claim is current (or its batch settled: nothing is left to renew).
    Renewed,
    /// The renewal did not complete within its timeout, or the database
    /// refused it: the lease runs on toward lapsing.
    Failed,
    /// Another poller holds the job: the claim ended.
    Lost,
}
impl Renewal {
    pub(crate) const ALL: [Self; 3] = [Self::Renewed, Self::Failed, Self::Lost];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Renewed => "renewed",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }
}

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
        jobs.poll_forever(owner, every).await
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

    fn observe_renewal(&self, outcome: Renewal) {
        #[cfg(test)]
        self.renewal_counts[outcome as usize].fetch_add(1, Ordering::SeqCst);
        crate::metrics::METRICS.observe_job_lease_renewal(outcome.as_str());
    }
    /// Renewals of this service's claims with `outcome` so far (tests).
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) fn renewals(&self, outcome: Renewal) -> usize {
        self.renewal_counts[outcome as usize].load(Ordering::SeqCst)
    }

    /// Jobs refreshed at once.
    fn poll_concurrency(&self) -> usize {
        self.limits.poll_concurrency.max(1)
    }

    /// One poll pass as an owner of its own; returns how many jobs were
    /// refreshed successfully.
    pub async fn poll_once(&self) -> Result<usize, InferenceError> {
        self.poll_as(Uuid::new_v4()).await
    }

    /// One bounded poll pass as `owner`: at most [`BATCH`] claims,
    /// [`Self::poll_concurrency`] refreshed at once, until a claim comes back
    /// short and every refresh ended. A failed claim stops claiming but lets
    /// the refreshes in flight end their claims before it is reported.
    pub(crate) async fn poll_as(&self, owner: Uuid) -> Result<usize, InferenceError> {
        let mut running = FuturesUnordered::new();
        let mut held = HashSet::new();
        let (mut claimed, mut exhausted, mut done, mut failed) = (0, false, 0, None);
        loop {
            let want = (self.poll_concurrency() - running.len()).min(BATCH - claimed);
            if !exhausted && want > 0 {
                match self.claim(owner, want, &held).await {
                    Ok(jobs) => {
                        exhausted = jobs.len() < want;
                        claimed += jobs.len();
                        for (job, fence) in jobs {
                            held.insert(fence.job);
                            running.push(self.guarded(job, fence));
                        }
                    }
                    Err(e) => {
                        exhausted = true;
                        failed = Some(e);
                    }
                }
            }
            match running.next().await {
                Some((job, refreshed)) => {
                    held.remove(&job);
                    done += usize::from(refreshed);
                }
                None => return failed.map_or(Ok(done), Err),
            }
        }
    }

    /// The poller as `owner`: claim as many due jobs as there are free slots
    /// every `every`, and again whenever a refresh frees a slot, and refresh
    /// them alongside the refreshes still running. A claim is one future
    /// polled beside the refreshes, so a claim that waits on the database
    /// never stops their lease renewals, and it never claims a job this
    /// poller is still refreshing.
    pub(crate) async fn poll_forever(&self, owner: Uuid, every: Duration) {
        type Claim<'a> = std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<(JobRow, BatchFence)>, InferenceError>> + Send + 'a>,
        >;
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut running = FuturesUnordered::new();
        let mut held = HashSet::new();
        let mut claiming: Option<Claim<'_>> = None;
        let mut refill = false;
        loop {
            let free = self.poll_concurrency().saturating_sub(running.len());
            if claiming.is_none() && refill && free > 0 {
                let excluded = held.clone();
                claiming = Some(Box::pin(
                    async move { self.claim(owner, free, &excluded).await },
                ));
            }
            refill = false;
            tokio::select! {
                _ = tick.tick() => refill = true,
                claimed = async { claiming.as_mut().expect("guarded by the branch condition").await }, if claiming.is_some() => {
                    claiming = None;
                    match claimed {
                        Ok(jobs) => {
                            for (job, fence) in jobs {
                                held.insert(fence.job);
                                running.push(self.guarded(job, fence));
                            }
                        }
                        Err(_) => tracing::warn!("async job poll incomplete; retrying next interval"),
                    }
                }
                Some((job, _)) = running.next(), if !running.is_empty() => {
                    held.remove(&job);
                    refill = true;
                }
            }
        }
    }

    /// Claim up to `limit` due jobs for `owner` in one statement, never one
    /// in `held` (refreshes this poller still runs).
    async fn claim(
        &self,
        owner: Uuid,
        limit: usize,
        held: &HashSet<Uuid>,
    ) -> Result<Vec<(JobRow, BatchFence)>, InferenceError> {
        #[cfg(test)]
        {
            self.claim_hold.pause().await;
            let n = self.claim_statements.fetch_add(1, Ordering::SeqCst) + 1;
            if self.failing_claim.load(Ordering::SeqCst) == n {
                return Err(InferenceError::Storage);
            }
            if take_one(&self.empty_claims) {
                return Ok(Vec::new());
            }
        }
        let lease = self.poll_lease();
        let held: Vec<Uuid> = held.iter().copied().collect();
        store::claim_due_many(
            &self.store,
            owner,
            self.poll_seconds(),
            lease.seconds,
            limit,
            &held,
        )
        .await
        .map_err(|_| InferenceError::Storage)
    }

    /// [`Self::refresh`], with a panic kept to its own job: the poller goes
    /// on with the others, and the panicked claim lapses after its lease.
    /// The job id and whether the refresh succeeded.
    async fn guarded(&self, job: JobRow, fence: BatchFence) -> (Uuid, bool) {
        let id = fence.job;
        let refreshed = std::panic::AssertUnwindSafe(self.refresh(job, fence))
            .catch_unwind()
            .await;
        if refreshed.is_err() {
            tracing::error!(job_id = %id, "async job refresh panicked; its claim lapses");
        }
        (id, refreshed.unwrap_or(false))
    }

    /// Refresh one claimed job, renewing its lease, then end the claim
    /// (backing off after a failure). Whether the refresh succeeded.
    async fn refresh(&self, job: JobRow, fence: BatchFence) -> bool {
        #[cfg(test)]
        assert!(
            !take_one(&self.panicking_refreshes),
            "a refresh panics (test)"
        );
        let lease = self.poll_lease();
        let lost = AtomicBool::new(false);
        let refreshed = tokio::time::timeout(REFRESH_TIMEOUT, self.poll_job(job, fence, &lost));
        let refreshed = self.renewing(fence, lease, &lost, refreshed).await;
        let ok = matches!(refreshed, Ok(Ok(())));
        if !ok {
            tracing::warn!(job_id = %fence.job, "async job refresh failed; backing off");
        }
        // A refused end is the normal case of a claim another poller took.
        let _ = fence
            .end_poll(&self.store, (!ok).then(|| self.poll_seconds()))
            .await;
        ok
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
                Ok(Ok(Renewed::Held { .. })) => self.observe_renewal(Renewal::Renewed),
                Ok(Ok(Renewed::Settled)) => {
                    self.observe_renewal(Renewal::Renewed);
                    return;
                }
                Ok(Ok(Renewed::Fenced)) => {
                    self.observe_renewal(Renewal::Lost);
                    lost.store(true, Ordering::SeqCst);
                    return;
                }
                _ => {
                    self.observe_renewal(Renewal::Failed);
                    tracing::warn!(job_id = %fence.job, "async job lease renewal failed");
                }
            }
        }
    }

    async fn poll_job(&self, job: JobRow, fence: BatchFence, lost: &AtomicBool) -> JobResult<()> {
        match job.job_kind() {
            Some(JobKind::Video) => {
                self.refresh_video_until(job, store::Observer::Claim(&fence), lost)
                    .await?;
            }
            Some(JobKind::Batch) => match job.mode() {
                Some(BatchMode::Native) => self.poll_native(job, fence, lost).await?,
                // Gateway-run batches belong to the batch runner.
                Some(BatchMode::Gateway) => {}
                None => {
                    self.refresh_legacy(job, store::Observer::Claim(&fence))
                        .await?;
                }
            },
            None => return Err(InferenceError::Storage.into()),
        }
        Ok(())
    }
}

/// Take one from a test counter if it is above zero (tests).
#[cfg(test)]
fn take_one(n: &std::sync::atomic::AtomicUsize) -> bool {
    let mut current = n.load(Ordering::SeqCst);
    while current > 0 {
        match n.compare_exchange(current, current - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => current = seen,
        }
    }
    false
}
