//! Two pollers on one native batch: the poller's claim is an owner, an epoch
//! and a renewable lease on the job row, and every write the poller makes to a
//! native batch checks that claim in its own transaction. A second poller
//! never refreshes a batch another holds; a poller whose claim ended writes
//! nothing and removes the result files it created.
use std::{sync::atomic::AtomicBool, time::Duration};

use super::*;
use crate::jobs::{fence::BatchFence, poller::PollLease};

/// A native batch of two lines, submitted, with a published batch list.
async fn submitted(pool: PgPool) -> (World, JobRow) {
    let w = world(pool, true, false, 2).await;
    v3_price(&w.f, w.f.deployment, true).await;
    let file = w
        .upload(
            &w.f.principal,
            &[
                chat_line("first", "company/smart", "one"),
                chat_line("second", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    w.poll().await;
    let job = w.reload(&job).await;
    assert!(job.upstream_id.is_some());
    (w, job)
}

/// The batch is due again, as it is once the claim's poll interval passed.
async fn due(w: &World, job: &JobRow) {
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// The poller that holds the batch stops renewing: its lease lapses.
async fn lapse(w: &World, job: &JobRow) {
    sqlx::query(
        "UPDATE async_jobs SET runner_lease_until=now()-interval '1 second',next_poll_at=now() WHERE id=$1",
    )
    .bind(job.id)
    .execute(&w.f.store.pool)
    .await
    .unwrap();
}

/// Committed, undeleted result files of the workspace.
async fn live_results(w: &World) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM stored_files WHERE workspace_id=$1 AND api_purpose='batch_output' AND committed_at IS NOT NULL AND deleted_at IS NULL ORDER BY id")
        .bind(w.f.principal.workspace_id)
        .fetch_all(&w.f.store.pool)
        .await
        .unwrap()
}

fn completed(w: &World) {
    let mut s = w.openai.script.lock().unwrap();
    s.batches.push_back(upstream(BatchStatus::Completed));
    s.batches.push_back(upstream(BatchStatus::Completed));
    s.results = vec![
        json!({"custom_id":"l0","ok":true,"input":30,"output":6}),
        json!({"custom_id":"l1","ok":false}),
    ];
}

fn gate(slot: &mut Option<Arc<Gate>>) -> Arc<Gate> {
    slot.insert(Arc::default()).clone()
}

fn count(calls: &[&str], call: &str) -> usize {
    calls.iter().filter(|c| **c == call).count()
}

#[dbtest::test]
async fn a_second_poller_never_refreshes_a_native_batch_another_holds(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    // Poller A reaches collection and reads the results for a long time.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // Its batch is due again while A still works on it: poller B leaves it.
    due(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 0);
    assert_eq!(count(&w.openai.calls(), "retrieve"), 1);
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 1);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    let mut linked: Vec<Uuid> = [job.output_file_id, job.error_file_id]
        .into_iter()
        .flatten()
        .collect();
    linked.sort_unstable();
    assert_eq!(live_results(&w).await, linked);
    assert_eq!(count(&w.openai.calls(), "delete"), 1);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_poller_that_lost_its_claim_writes_nothing_and_removes_its_result_files(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // A stalls past its lease: poller B takes the batch over and collects it.
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    let collected = w.reload(&job).await;
    assert_eq!(collected.state, "completed");
    assert!(collected.settled_at.is_some());
    let settled = reservation(&w.f, job.execution_id).await;
    // A resumes: it records nothing and deletes the files it wrote.
    held.release.notify_one();
    a.await.unwrap().unwrap();
    let job = w.reload(&job).await;
    assert_eq!(
        (job.output_file_id, job.error_file_id),
        (collected.output_file_id, collected.error_file_id)
    );
    let mut linked: Vec<Uuid> = [job.output_file_id, job.error_file_id]
        .into_iter()
        .flatten()
        .collect();
    linked.sort_unstable();
    assert_eq!(linked.len(), 2);
    assert_eq!(live_results(&w).await, linked);
    assert_eq!(reservation(&w.f, job.execution_id).await, settled);
    // Only the poller that holds the batch deletes the provider's copies.
    assert_eq!(count(&w.openai.calls(), "delete"), 1);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_poller_that_lost_its_claim_never_overwrites_newer_progress(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    let progress = |completed| {
        let mut b = upstream(BatchStatus::InProgress);
        b.counts = Some(RequestCounts {
            total: 2,
            completed,
            failed: 0,
        });
        b
    };
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        s.batches.push_back(progress(1));
        s.batches.push_back(progress(2));
        gate(&mut s.retrieve_gate)
    };
    due(&w, &job).await;
    // A reads one finished line, then stalls before it records it.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    lapse(&w, &job).await;
    // B takes the batch over and records two.
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    assert_eq!(w.reload(&job).await.request_completed, Some(2));
    held.release.notify_one();
    a.await.unwrap().unwrap();
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.upstream_status.as_deref()),
        (Some(2), Some("in_progress"))
    );
}

/// Another poller (`owner`) takes the batch over: the holder's lease lapses
/// and `owner` claims it. Retried while a renewal of the holder's lease wins
/// the race against the lapse.
async fn take_over(w: &World, job: &JobRow, owner: Uuid) -> BatchFence {
    for _ in 0..50 {
        lapse(w, job).await;
        if let Some((_, fence)) = store::claim_due(&w.f.store, owner, 30.0, 60.0)
            .await
            .unwrap()
        {
            assert_eq!(fence.job, job.id);
            return fence;
        }
    }
    panic!("the batch was never taken over");
}

/// The claim columns of the job row (timestamps as text, compared exactly).
async fn claim(w: &World, job: &JobRow) -> (Option<Uuid>, Option<String>, i32, String) {
    sqlx::query_as("SELECT runner_id,runner_lease_until::text,poll_failures,next_poll_at::text FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap()
}

/// Every result file of the workspace, committed or not, deleted or not.
async fn all_results(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE workspace_id=$1 AND api_purpose='batch_output'",
    )
    .bind(w.f.principal.workspace_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap()
}

/// A short lease, renewed several times a second. A stalled runner has the
/// lease term less one renewal period before a renewal comes too late.
const SHORT: PollLease = PollLease {
    seconds: 2.0,
    renew: Duration::from_millis(250),
};

#[dbtest::test]
async fn the_lease_is_renewed_while_a_collection_outlives_it(pool: PgPool) {
    let (mut w, job) = submitted(pool).await;
    w.jobs.poll_lease = SHORT;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // A reads the results for longer than two lease terms.
    tokio::time::sleep(Duration::from_secs_f64(SHORT.seconds * 2.5)).await;
    let live: bool = sqlx::query_scalar(
        "SELECT runner_lease_until>clock_timestamp() FROM async_jobs WHERE id=$1",
    )
    .bind(job.id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert!(live, "the lease was renewed");
    // Without the renewals the lease would have lapsed and B would claim it.
    due(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 0);
    assert_eq!(count(&w.openai.calls(), "retrieve"), 1);
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 1);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_failed_refresh_releases_the_claim_and_backs_off(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    // No provider observation is scripted: the retrieval fails.
    let held = gate(&mut w.openai.script.lock().unwrap().retrieve_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    let (_, lease, failures, claimed_next) = claim(&w, &job).await;
    assert!(lease.is_some());
    assert_eq!(failures, 0);
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 0);
    let (_, lease, failures, next) = claim(&w, &job).await;
    assert_eq!((lease, failures), (None, 1));
    let later: bool = sqlx::query_scalar("SELECT $1::timestamptz>$2::timestamptz")
        .bind(&next)
        .bind(&claimed_next)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert!(later, "backed off past the claim: {next} > {claimed_next}");
}

#[dbtest::test]
async fn a_stale_end_of_poll_leaves_the_holders_claim_alone(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    let held = gate(&mut w.openai.script.lock().unwrap().retrieve_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // B holds the batch while A's retrieval is still open.
    let b = Uuid::new_v4();
    take_over(&w, &job, b).await;
    let holder = claim(&w, &job).await;
    assert_eq!(holder.0, Some(b));
    assert!(holder.1.is_some());
    // A's retrieval fails: its end of poll (release and backoff) is refused.
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 0);
    assert_eq!(claim(&w, &job).await, holder);
}

#[dbtest::test]
async fn a_terminal_unsettled_batch_is_collected_again_without_new_links(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    w.jobs.settle_hold.arm();
    due(&w, &job).await;
    // A records the results, then stops before it settles.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    w.jobs.settle_hold.reached().await;
    let first = w.reload(&job).await;
    assert_eq!(first.state, "completed");
    assert!(first.settled_at.is_none());
    let mut linked: Vec<Uuid> = [first.output_file_id, first.error_file_id]
        .into_iter()
        .flatten()
        .collect();
    linked.sort_unstable();
    assert_eq!(linked.len(), 2);
    // A's lease lapses: B collects the batch again, keeps A's links,
    // deletes its own result files and settles.
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    let second = w.reload(&job).await;
    assert_eq!(
        (second.output_file_id, second.error_file_id),
        (first.output_file_id, first.error_file_id)
    );
    assert!(second.settled_at.is_some());
    assert_eq!(all_results(&w).await, 4, "B wrote a second set");
    assert_eq!(live_results(&w).await, linked);
    let settled = reservation(&w.f, job.execution_id).await;
    assert_eq!(settled.0, "settled");
    // A resumes after its finish committed: settlement and deletion of the
    // provider's copies are not fenced. Its settlement changes nothing.
    w.jobs.settle_hold.release();
    a.await.unwrap().unwrap();
    assert_eq!(reservation(&w.f, job.execution_id).await, settled);
    assert_eq!(live_results(&w).await, linked);
    assert_eq!(count(&w.openai.calls(), "delete"), 2);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_poller_whose_renewal_is_refused_stops_collecting_before_it_writes(pool: PgPool) {
    let (mut w, job) = submitted(pool).await;
    w.jobs.poll_lease = SHORT;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    let b = Uuid::new_v4();
    take_over(&w, &job, b).await;
    let holder = claim(&w, &job).await;
    // A's next renewals are refused: it knows the claim is lost before it
    // reads a result line.
    tokio::time::sleep(SHORT.renew * 12).await;
    held.release.notify_one();
    a.await.unwrap().unwrap();
    assert_eq!(all_results(&w).await, 0, "no result file was opened");
    let job = w.reload(&job).await;
    assert_eq!(job.state, "queued");
    assert!(job.settled_at.is_none());
    assert_eq!(claim(&w, &job).await, holder);
    assert_eq!(count(&w.openai.calls(), "delete"), 0);
}

#[dbtest::test]
async fn a_requested_cancel_keeps_cancelling_against_an_older_provider_status(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        s.batches.push_back(upstream(BatchStatus::InProgress));
        gate(&mut s.retrieve_gate)
    };
    due(&w, &job).await;
    // A reads `in_progress`; the client then cancels the batch.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    let v = w
        .jobs
        .cancel_batch(&w.f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["status"], "cancelling");
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 1);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "in_progress");
    assert_eq!(job.upstream_status.as_deref(), Some("cancelling"));
}

/// A native batch of one line, created but not yet submitted.
async fn unsubmitted(pool: PgPool) -> (World, JobRow) {
    let w = world(pool, true, false, 1).await;
    v3_price(&w.f, w.f.deployment, true).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    (w, job)
}

#[dbtest::test]
async fn a_poller_that_lost_its_claim_never_submits(pool: PgPool) {
    let (w, job) = unsubmitted(pool).await;
    let a = Uuid::new_v4();
    let (claimed, fence) = store::claim_due(&w.f.store, a, 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    take_over(&w, &job, Uuid::new_v4()).await;
    w.jobs
        .poll_native(claimed, fence, &AtomicBool::new(false))
        .await
        .unwrap();
    assert_eq!(count(&w.openai.calls(), "submit"), 0);
    let job = w.reload(&job).await;
    assert!(job.submit_started_at.is_none() && job.upstream_id.is_none());
    assert_eq!(job.state, "queued");
}

#[dbtest::test]
async fn a_submission_that_fails_after_its_claim_ended_writes_and_settles_nothing(pool: PgPool) {
    let (mut w, job) = unsubmitted(pool).await;
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        // A refusal that would otherwise take the free release.
        s.submission_error = Some(InferenceError::UpstreamRejected);
        gate(&mut s.submit_gate)
    };
    let before = reservation(&w.f, job.execution_id).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    take_over(&w, &job, Uuid::new_v4()).await;
    held.release.notify_one();
    a.await.unwrap().unwrap();
    assert_eq!(count(&w.openai.calls(), "submit"), 1);
    let stale = w.reload(&job).await;
    assert_eq!(stale.state, "queued");
    assert!(stale.submit_started_at.is_some() && stale.upstream_id.is_none());
    assert!(stale.error_code.is_none() && stale.settled_at.is_none());
    assert_eq!(reservation(&w.f, job.execution_id).await, before);
    // The poller that holds the batch ends it interrupted after the grace,
    // with the hold kept as unknown cost.
    w.jobs.submit_grace = TimeDelta::minutes(0);
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    let ended = w.reload(&job).await;
    assert_eq!(
        (ended.state.as_str(), ended.error_code.as_deref()),
        ("failed", Some("submission_interrupted"))
    );
    assert!(ended.settled_at.is_some());
    // Not the free release: the whole hold stays, as unknown cost.
    let kept = reservation(&w.f, job.execution_id).await;
    assert_eq!(
        (kept.0.as_str(), kept.1, kept.2),
        ("unknown", before.1, None)
    );
    assert_eq!(before.0, "pending");
    assert_eq!(count(&w.openai.calls(), "submit"), 1);
    consistent(&w.f).await;
}

/// A batch's line counts and finished-batch metrics are counted once, by the
/// claim that recorded its finish: not again by a collection that finds it
/// finished, nor by the first poller's late settlement.
#[dbtest::test]
async fn a_collected_batch_is_counted_once_however_often_it_is_collected(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    let counted = || {
        w.jobs
            .finish_metrics
            .load(std::sync::atomic::Ordering::SeqCst)
    };
    let before = counted();
    w.jobs.settle_hold.arm();
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    w.jobs.settle_hold.reached().await;
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    w.jobs.settle_hold.release();
    a.await.unwrap().unwrap();
    assert!(w.reload(&job).await.settled_at.is_some());
    assert_eq!(counted() - before, 1);
}

/// A batch whose finish committed and whose settlement did not is settled
/// from the provider's aggregate usage, when it reports one, without
/// downloading its results again.
#[dbtest::test]
async fn a_finished_batch_with_aggregate_usage_settles_without_a_second_download(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    {
        let mut s = w.openai.script.lock().unwrap();
        for _ in 0..2 {
            let mut done = upstream(BatchStatus::Completed);
            done.usage = Some(usage(30, 6));
            s.batches.push_back(done);
        }
        s.results = vec![
            json!({"custom_id":"l0","ok":true,"input":30,"output":6}),
            json!({"custom_id":"l1","ok":false}),
        ];
    }
    w.jobs.settle_hold.arm();
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    w.jobs.settle_hold.reached().await;
    let downloads = count(&w.openai.calls(), "results");
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    assert_eq!(
        count(&w.openai.calls(), "results"),
        downloads,
        "no second download"
    );
    assert_eq!(all_results(&w).await, 2, "no second set of result files");
    let settled = reservation(&w.f, job.execution_id).await;
    assert_eq!(settled.0, "settled");
    w.jobs.settle_hold.release();
    a.await.unwrap().unwrap();
    assert_eq!(reservation(&w.f, job.execution_id).await, settled);
    consistent(&w.f).await;
}

/// A submission that the provider accepts only after another poller ended
/// the batch `submission_interrupted` records no upstream id on the settled
/// row: the late id is counted (and logged) instead, so the provider batch
/// that runs unpolled is visible. The batch keeps its retained hold.
#[dbtest::test]
async fn a_late_accepted_submission_is_counted_not_recorded_on_an_ended_batch(pool: PgPool) {
    let (mut w, job) = unsubmitted(pool).await;
    w.jobs.submit_grace = TimeDelta::minutes(0);
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        gate(&mut s.submit_gate)
    };
    let late = || crate::metrics::METRICS.batch_event_count("native", "late_submission");
    let before = late();
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // A's lease lapses while its submission is in flight; B ends the batch
    // as interrupted (the grace has passed) and settles it.
    lapse(&w, &job).await;
    w.jobs.poll_once().await.unwrap();
    let ended = w.reload(&job).await;
    assert_eq!(
        (ended.state.as_str(), ended.error_code.as_deref()),
        ("failed", Some("submission_interrupted"))
    );
    assert!(ended.settled_at.is_some());
    let settled = reservation(&w.f, job.execution_id).await;
    // The provider accepts A's submission now.
    held.release.notify_one();
    a.await.unwrap().unwrap();
    let after = w.reload(&job).await;
    assert_eq!(after.upstream_id, None, "no id on an ended batch");
    assert_eq!(after.state, "failed");
    assert_eq!(late() - before, 1);
    assert_eq!(reservation(&w.f, job.execution_id).await, settled);
    consistent(&w.f).await;
}

/// Lease renewals are counted by outcome: a held claim renews, and a renewal
/// refused after a takeover is counted as lost (once: the renewals stop).
#[dbtest::test]
async fn lease_renewals_are_counted_by_outcome(pool: PgPool) {
    use crate::jobs::poller::Renewal;
    let (mut w, job) = submitted(pool).await;
    w.jobs.poll_lease = SHORT;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    tokio::time::sleep(SHORT.renew * 4).await;
    assert!(
        w.jobs.renewals(Renewal::Renewed) >= 1,
        "a held claim renews"
    );
    assert_eq!(w.jobs.renewals(Renewal::Lost), 0);
    take_over(&w, &job, Uuid::new_v4()).await;
    tokio::time::sleep(SHORT.renew * 12).await;
    held.release.notify_one();
    a.await.unwrap().unwrap();
    assert_eq!(w.jobs.renewals(Renewal::Lost), 1);
}

/// A poller that finds a started submission past its grace does not end the
/// batch interrupted when the provider's id landed in the meantime (under
/// the finish lock): the batch is polled as submitted.
#[dbtest::test]
async fn an_upstream_id_that_lands_first_keeps_the_batch_from_being_interrupted(pool: PgPool) {
    let (mut w, job) = unsubmitted(pool).await;
    w.jobs.submit_grace = TimeDelta::minutes(0);
    sqlx::query("UPDATE async_jobs SET submit_started_at=now()-interval '1 hour',next_poll_at=now() WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    // B's claim reads the batch without an upstream id ...
    let (claimed, fence) = store::claim_due(&w.f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    assert!(claimed.upstream_id.is_none());
    // ... then the slow submitter records the provider's id.
    assert!(
        store::set_upstream(
            &w.f.store,
            job.id,
            &UpstreamId::parse("batch_late").unwrap()
        )
        .await
        .unwrap()
    );
    w.jobs
        .poll_native(claimed, fence, &AtomicBool::new(false))
        .await
        .unwrap();
    let after = w.reload(&job).await;
    assert_eq!(after.state, "queued", "not interrupted");
    assert_eq!(after.upstream_id.as_deref(), Some("batch_late"));
    assert!(after.settled_at.is_none());
}
