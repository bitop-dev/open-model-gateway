//! A gateway-run batch's claim is a fence: once another runner has taken a
//! batch over, the runner that lost it writes nothing more to it.
use super::*;
use crate::jobs::fence::{FenceError, Renewed, SegmentLines};
use crate::jobs::schedule::{Scheduler, claims::RetryClaim};

/// How long a test waits for a line to reach the provider. A transient
/// planning failure (`Storage` or `Timeout`) is treated like a route's
/// cooldown: the line is planned again only after one `jobs::runner::REPLAN`,
/// so a wait of exactly `REPLAN` can expire before the next plan reaches the
/// provider. Two `REPLAN`s and a margin still bound a stuck runner.
const REACH: Duration = crate::jobs::runner::REPLAN
    .saturating_mul(2)
    .saturating_add(Duration::from_secs(10));

/// `world` with one batch of `lines` (chat completions, one line each).
async fn batch(pool: PgPool, lines: &[(&str, &str)]) -> (World, JobRow) {
    let w = world(pool, false, false, 1).await;
    let lines: Vec<Value> = lines
        .iter()
        .map(|(id, text)| chat_line(id, "company/smart", text))
        .collect();
    let file = w.upload(&w.f.principal, &lines).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    (w, job)
}

/// Run the next claimable batch on a new runner (a process of its own).
pub(super) fn start(w: &World) -> tokio::task::JoinHandle<JobResult<bool>> {
    start_with(w.jobs.clone())
}

/// [`start`] with `jobs`, for a test that changes its settings.
fn start_with(jobs: Jobs) -> tokio::task::JoinHandle<JobResult<bool>> {
    let runner = Runner::new(jobs);
    tokio::spawn(async move { runner.run_next().await })
}

/// `w.jobs` flushing result segments only after `age`: a test asserting that
/// a finished line is still unflushed while a later line is planned sets an
/// age that the plan's own wait (up to [`REACH`]) cannot reach.
fn flushing_after(w: &World, age: Duration) -> Jobs {
    let mut jobs = w.jobs.clone();
    jobs.segment_age = Some(age);
    jobs
}

/// A replacement runner taking `job` over, as the runner's loop runs it. The
/// tests simulate the lease's expiry while the old runner still renews on its
/// own schedule, so before each claim the lease is lapsed again ([`lapse`]):
/// a renewal that lands before a claim would otherwise extend the lease (one
/// after the claim is refused: the epoch moved on). A claim also skips a
/// batch whose job row another transaction holds at that moment
/// (`claim_gateway` takes `FOR UPDATE SKIP LOCKED`), and the loop claims
/// again on its next tick. So an empty claim is retried, for at most
/// [`REACH`]; a run that claimed returns its own result.
fn start_replacement(
    w: &World,
    job: &JobRow,
    jobs: Jobs,
) -> tokio::task::JoinHandle<JobResult<bool>> {
    let runner = Runner::new(jobs);
    let (pool, id) = (w.f.store.pool.clone(), job.id);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + REACH;
        loop {
            lapse_lease(&pool, id).await;
            match runner.run_next().await {
                Ok(false) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                other => return other,
            }
        }
    })
}

/// The batch's lease lapses: it is backdated as if its runner had stopped
/// renewing. A runner still running renews on its own schedule, so a renewal
/// that lands before a replacement claims extends the lease again: a takeover
/// from a running runner uses [`start_replacement`].
pub(super) async fn lapse(w: &World, job: &JobRow) {
    lapse_lease(&w.f.store.pool, job.id).await;
}
async fn lapse_lease(pool: &PgPool, job: Uuid) {
    sqlx::query("UPDATE async_jobs SET runner_lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(job)
        .execute(pool)
        .await
        .unwrap();
}

async fn count(w: &World, sql: &str, job: &JobRow) -> i64 {
    sqlx::query_scalar(sql)
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap()
}

/// Live segment files of the batch.
async fn segments(w: &World, job: &JobRow) -> i64 {
    count(w, "SELECT count(*) FROM batch_segments s JOIN stored_files f ON f.id=s.file_id WHERE s.job_id=$1 AND f.deleted_at IS NULL", job).await
}

/// Stored files of the batch's workspace that are not deleted.
async fn live_files(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE workspace_id=$1 AND deleted_at IS NULL",
    )
    .bind(w.f.principal.workspace_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap()
}

async fn envelope(w: &World, job: &JobRow) -> String {
    reservation(&w.f, job.execution_id).await.0
}

/// The old runner's provider call returns after the new runner finished the
/// batch: its line is already `interrupted`, counted and reported unavailable.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_cannot_change_a_batch_the_new_owner_finished(pool: PgPool) {
    let log = crate::log_capture_test::Capture::start();
    let (w, job) = batch(pool, &[("a", "hold-a")]).await;
    let held = w.openai.gate("hold-a");
    let old = start(&w);
    held.entered.notified().await;
    // The new owner interrupts the line and finishes the batch.
    assert!(
        start_replacement(&w, &job, w.jobs.clone())
            .await
            .unwrap()
            .unwrap()
    );
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    assert_eq!(
        (
            done.request_total,
            done.request_completed,
            done.request_failed
        ),
        (Some(1), Some(0), Some(1))
    );
    let errors = w.read(w.f.principal.workspace_id, done.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["error"]["code"], "result_unavailable");
    let files = live_files(&w).await;

    // The old runner's call completes (and is charged): it may write nothing.
    held.release.notify_one();
    let old = old.await.unwrap();
    let after = w.reload(&job).await;
    assert_eq!(
        (after.request_completed, after.request_failed),
        (Some(0), Some(1)),
        "a line is counted once, and never beyond the total"
    );
    assert_eq!(
        (
            after.state.as_str(),
            after.output_file_id,
            after.error_file_id
        ),
        ("completed", done.output_file_id, done.error_file_id)
    );
    assert_eq!(segments(&w, &job).await, 0, "no result segment recorded");
    assert_eq!(live_files(&w).await, files, "no duplicate output files");
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT state,error_code FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, ("interrupted".into(), Some("interrupted".into())));
    // The attempt's reservation still settles at what the provider charged,
    // so the batch's cost and its result file agree: charged, unavailable.
    let attempt: (String, Option<i64>) = sqlx::query_as("SELECT r.state,r.actual_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(attempt, ("settled".into(), Some(15)));
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (15, 0));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    // The refused outcome is logged once, with ids only, so the charged line
    // can be attributed. Any other event naming the execution must be one of
    // the advisory events listed here: a routing-health write of its attempt
    // that failed or outlived its 250 ms timeout (`ExecutionGuard::finish`) is
    // logged with the same id.
    const NOT_RECORDED: &str =
        "batch line outcome not recorded: the runner no longer holds the batch";
    const ADVISORY: [&str; 1] = ["routing health update failed"];
    let execution: Uuid =
        sqlx::query_scalar("SELECT execution_id FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    let text = log.text();
    let events: Vec<Value> = text
        .lines()
        .filter(|l| l.contains(&execution.to_string()))
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not a log event: {l}")))
        .collect();
    let refused: Vec<&Value> = events
        .iter()
        .filter(|e| e["fields"]["message"] == NOT_RECORDED)
        .collect();
    assert_eq!(refused.len(), 1, "{text}");
    assert_eq!(
        refused[0]["fields"],
        json!({"message": NOT_RECORDED, "execution_id": execution.to_string(), "line": 0}),
        "ids only, of this execution and line: {text}"
    );
    for event in &events {
        let message = event["fields"]["message"].as_str().unwrap_or_default();
        assert!(
            message == NOT_RECORDED || ADVISORY.contains(&message),
            "an unexpected event names the execution: {event}"
        );
    }
    consistent(&w.f).await;
}

async fn overlapping_runners(
    w: &World,
    job: &JobRow,
) -> (
    tokio::task::JoinHandle<JobResult<bool>>,
    tokio::task::JoinHandle<JobResult<bool>>,
    Arc<Gate>,
    Arc<Gate>,
) {
    // Each runner executes one line. Both calls must overlap on this route.
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,updated_by) VALUES($1,2,$2)")
        .bind(w.f.deployment).bind(w.f.owner).execute(&w.f.store.pool).await.unwrap();
    let (held_a, held_b) = (w.openai.gate("hold-a"), w.openai.gate("hold-b"));
    let old = start(w);
    tokio::time::timeout(REACH, held_a.entered.notified())
        .await
        .expect("old runner reached the provider");
    let owner = start_replacement(w, job, w.jobs.clone());
    tokio::time::timeout(REACH, held_b.entered.notified())
        .await
        .expect("replacement runner used the second route slot");
    (old, owner, held_a, held_b)
}

/// The old runner returns while the new owner still runs: it must not finish
/// or settle the batch, and the owner's own results must all be recorded.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_neither_finalizes_it_nor_costs_the_owner_a_result(
    pool: PgPool,
) {
    let (w, job) = batch(pool, &[("a", "hold-a"), ("b", "hold-b")]).await;
    let (old, owner, held_a, held_b) = overlapping_runners(&w, &job).await;
    assert_eq!(w.reload(&job).await.request_failed, Some(1));

    held_a.release.notify_one();
    let old = old.await.unwrap();
    let running = w.reload(&job).await;
    assert_eq!(
        running.state, "in_progress",
        "the old runner finalized the batch"
    );
    assert_eq!(
        envelope(&w, &job).await,
        "pending",
        "the old runner closed the envelope"
    );
    assert_eq!(segments(&w, &job).await, 0);
    assert_eq!(
        (running.request_completed, running.request_failed),
        (Some(0), Some(1))
    );

    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let out = w.read(w.f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1, "the owner's result was lost");
    assert_eq!(out[0]["custom_id"], "b");
    let errors = w.read(w.f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["custom_id"], "a");
    assert_eq!(errors[0]["error"]["code"], "result_unavailable");
    assert_eq!(envelope(&w, &job).await, "settled");
    consistent(&w.f).await;
}

/// A runner that lost the batch starts no further line, even one nobody
/// claimed yet.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_claims_and_runs_no_further_line(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "hold-a"), ("b", "hold-b"), ("c", "plain-c")]).await;
    let (old, owner, held_a, held_b) = overlapping_runners(&w, &job).await;

    held_a.release.notify_one();
    let old = old.await.unwrap();
    assert_eq!(
        w.openai.executed(),
        ["hold-a", "hold-b"],
        "the old runner started a line after it lost the batch"
    );
    assert_eq!(
        count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await,
        2
    );

    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    assert_eq!(w.openai.executed(), ["hold-a", "hold-b", "plain-c"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(1))
    );
    consistent(&w.f).await;
}

/// One transient planning failure right after a takeover (#835): the
/// replacement runner's first planning read of routing health is cancelled
/// while `deployment_health` is locked, so planning fails with `Storage`. The
/// line waits one `REPLAN` (a transient failure is treated like a cooldown),
/// then still reaches the provider, and the runner that lost the batch still
/// starts nothing more.
#[dbtest::test]
async fn a_planning_failure_after_a_takeover_delays_the_line_one_replan(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "hold-a"), ("b", "hold-b"), ("c", "plain-c")]).await;
    let db = &w.f.store.pool;
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,updated_by) VALUES($1,2,$2)")
        .bind(w.f.deployment).bind(w.f.owner).execute(db).await.unwrap();
    let (held_a, held_b) = (w.openai.gate("hold-a"), w.openai.gate("hold-b"));
    let old = start(&w);
    tokio::time::timeout(REACH, held_a.entered.notified())
        .await
        .expect("old runner reached the provider");
    let mut lock = db.begin().await.unwrap();
    sqlx::query("LOCK TABLE deployment_health IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    // The replacement plans on a pool of its own, named, so the statement
    // cancelled below is its planning read and never the old runner's (which
    // plans its next line too, though it can no longer start it).
    const REPLACEMENT: &str = "omg-fence-replacement";
    let replacement_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .idle_timeout(Some(Duration::from_secs(1)))
        .connect_with(
            (*db.connect_options())
                .clone()
                .application_name(REPLACEMENT),
        )
        .await
        .unwrap();
    let mut jobs = w.jobs.clone();
    jobs.store.pool = replacement_pool.clone();
    let owner = start_replacement(&w, &job, jobs);
    // The replacement runner's planning read, waiting for the lock: cancel it.
    let planning: i32 = tokio::time::timeout(REACH, async {
        loop {
            if let Some(pid) = sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND application_name=$2 AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%deployment_health%'")
                .bind(holder).bind(REPLACEMENT).fetch_optional(db).await.unwrap() {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the replacement runner's planning read reached the locked table");
    // Taken before the cancel is sent: the plan fails after the cancel is
    // issued, `head.planned` is set after the plan fails, and the line is
    // planned again no earlier than `planned + REPLAN` (all monotonic), so
    // cancel issued <= failed plan <= replan - REPLAN.
    let cancelled = std::time::Instant::now();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT pg_cancel_backend($1)")
            .bind(planning)
            .fetch_one(db)
            .await
            .unwrap()
    );
    tokio::time::timeout(REACH, async {
        while sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE pid=$1 AND $2=ANY(pg_blocking_pids(pid)))")
            .bind(planning).bind(holder).fetch_one(db).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the planning read was cancelled");
    lock.commit().await.unwrap();
    tokio::time::timeout(REACH, held_b.entered.notified())
        .await
        .expect("replacement runner used the second route slot after a planning failure");
    // From the cancel to the line's provider call: one REPLAN at least.
    assert!(
        cancelled.elapsed() >= crate::jobs::runner::REPLAN,
        "the line was planned again before one REPLAN: {:?}",
        cancelled.elapsed()
    );

    held_a.release.notify_one();
    let old = old.await.unwrap();
    assert_eq!(
        w.openai.executed(),
        ["hold-a", "hold-b"],
        "the old runner started a line after it lost the batch"
    );
    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    assert_eq!(w.openai.executed(), ["hold-a", "hold-b", "plain-c"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(1))
    );
    replacement_pool.close().await;
}

use crate::{
    governance::batch::{LineContext, admit_line},
    inference::{
        repository::{ExecutionStart, InferenceRepository},
        workload::{OutputReservation, WorkloadAdmission},
    },
    jobs::{fence::BatchFence, store::claim_gateway},
};

fn stale<T>(result: Result<T, FenceError>) -> bool {
    matches!(result, Err(FenceError::Stale))
}

/// Claim the batch as a new runner (lease already lapsed).
async fn claim(w: &World, job: &JobRow) -> BatchFence {
    lapse(w, job).await;
    let (_, fence) = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap()
        .pop()
        .expect("the batch is claimable");
    fence
}

#[dbtest::test]
async fn every_claim_of_a_batch_moves_its_epoch_on(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let first = claim(&w, &job).await;
    let second = claim(&w, &job).await;
    assert_eq!((first.epoch, second.epoch), (1, 2));
    // The same process claiming again is a new claim too: the earlier run of
    // it is fenced, never confused with the new one.
    lapse(&w, &job).await;
    let again = claim_gateway(&w.f.store, second.runner, 60.0, 1, &[])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .1;
    assert_eq!((again.runner, again.epoch), (second.runner, 3));
    assert_eq!(
        first.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Fenced
    );
    assert_eq!(
        second.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Fenced
    );
    assert_eq!(
        again.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Held { cancel: false }
    );
    // A run in progress is not claimed again by its own process.
    lapse(&w, &job).await;
    let none = claim_gateway(&w.f.store, again.runner, 60.0, 1, &[job.id])
        .await
        .unwrap();
    assert!(none.is_empty());
    // Epochs never move back.
    let back = sqlx::query("UPDATE async_jobs SET runner_epoch=1 WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await;
    assert!(back.is_err());

    // A renewal reaching the database after release cannot postpone takeover.
    again.release(&w.f.store).await.unwrap();
    assert_eq!(
        again.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Fenced
    );
    let replacements = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].1.epoch, again.epoch + 1);
}

#[dbtest::test]
async fn a_stale_claim_writes_nothing_and_a_current_one_everything(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain"), ("c", "plain")]).await;
    let old = claim(&w, &job).await;
    assert!(
        old.claim_line(&w.f.store, job.workspace_id, 0, Uuid::new_v4())
            .await
            .unwrap()
    );
    let new = claim(&w, &job).await;
    // The takeover interrupts the line the old claim left running.
    assert_eq!(new.interrupt_running(&w.f.store).await.unwrap(), 1);
    let s = &w.f.store;
    assert!(stale(
        old.claim_line(s, job.workspace_id, 1, Uuid::new_v4()).await
    ));
    assert!(stale(
        Scheduler::new(s.clone(), None)
            .claim_retry(&old, 0, 1, Uuid::new_v4(), None)
            .await
    ));
    assert!(stale(
        old.fail_line(s, 0, Uuid::nil(), Some(500), Some("failed"), true)
            .await
    ));
    assert!(stale(old.progress(s, 1, 0).await));
    assert!(stale(old.interrupt_running(s).await));
    assert!(stale(
        old.insert_segment(
            s,
            job.workspace_id,
            Uuid::new_v4(),
            SegmentLines {
                failed: &[(0, Uuid::nil())],
                ..Default::default()
            }
        )
        .await
    ));
    assert!(stale(old.set_status(s, "finalizing").await));
    assert!(stale(old.release(s).await));
    let done = crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: None,
        error_file: None,
        counts: None,
    };
    assert!(stale(old.finish_batch(s, &done).await));
    assert_eq!(
        crate::governance::batch::close_gateway_batch(s, job.execution_id, 1, &old)
            .await
            .unwrap(),
        crate::governance::batch::Closed::Fenced
    );
    assert_eq!(envelope(&w, &job).await, "pending");
    let row = w.reload(&job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.request_completed,
            row.request_failed
        ),
        ("queued", Some(0), Some(1)),
        "only the takeover's own interruption is counted"
    );
    // Only the matching running attempt can commit the segment and progress.
    let first_execution = Uuid::new_v4();
    let execution = Uuid::new_v4();
    let file = job.work_file_id.unwrap();
    assert!(
        new.claim_line(s, job.workspace_id, 1, first_execution)
            .await
            .unwrap()
    );
    assert!(
        new.fail_line(s, 1, first_execution, Some(503), Some("unavailable"), false)
            .await
            .unwrap()
    );
    assert_eq!(
        Scheduler::new(s.clone(), None)
            .claim_retry(&new, 1, 1, execution, None)
            .await
            .unwrap(),
        RetryClaim::Started
    );
    let second = Uuid::new_v4();
    assert!(
        new.claim_line(s, job.workspace_id, 2, second)
            .await
            .unwrap()
    );
    assert!(!new.await_segment(s, 1, first_execution).await.unwrap());
    assert!(new.await_segment(s, 1, execution).await.unwrap());
    assert!(new.await_segment(s, 2, second).await.unwrap());
    let record = |execution| async move {
        new.insert_segment(
            s,
            job.workspace_id,
            file,
            SegmentLines {
                succeeded: &[(1, execution), (2, second)],
                ..Default::default()
            },
        )
        .await
    };
    assert!(matches!(
        record(first_execution).await,
        Err(FenceError::Storage)
    ));
    assert_eq!(w.reload(&job).await.request_completed, Some(0));
    assert_eq!(segments(&w, &job).await, 0);
    assert_eq!(record(execution).await.unwrap(), 0);
    assert!(matches!(record(execution).await, Err(FenceError::Storage)));
    assert_eq!(w.reload(&job).await.request_completed, Some(2));
    assert_eq!(segments(&w, &job).await, 1);
}

/// A process that predates the epoch (or a stale writer) took the next
/// segment number: the owner takes the one after it and keeps flushing.
#[dbtest::test]
async fn a_taken_segment_number_never_blocks_the_owners_later_flushes(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain"), ("c", "plain")]).await;
    let fence = claim(&w, &job).await;
    let file = |n: u8| {
        let w = &w;
        async move {
            let stream: crate::filestore::ByteStream =
                Box::pin(futures_util::stream::once(async move {
                    Ok(axum::body::Bytes::from(vec![n]))
                }));
            w.files
                .create(
                    NewFile {
                        created_by_api_key_id: Some(w.f.principal.key_id),
                        quota: QuotaMode::CountOnly,
                        ..NewFile::new(Purpose::BatchOutput, Some(w.f.principal.workspace_id))
                    },
                    stream,
                )
                .await
                .unwrap()
                .id
        }
    };
    let ws = job.workspace_id;
    sqlx::query(
        "INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) VALUES($1,$2,0,$3,1)",
    )
    .bind(job.id)
    .bind(ws)
    .bind(file(0).await)
    .execute(&w.f.store.pool)
    .await
    .unwrap();
    let mut seqs = vec![];
    for n in 1..=3u8 {
        let line = i32::from(n) - 1;
        let execution = Uuid::new_v4();
        let store = &w.f.store;
        assert!(fence.claim_line(store, ws, line, execution).await.unwrap());
        assert!(
            fence
                .fail_line(store, line, execution, Some(500), Some("failed"), true)
                .await
                .unwrap()
        );
        let file_id = file(n).await;
        assert!(matches!(
            fence
                .insert_segment(
                    store,
                    ws,
                    file_id,
                    SegmentLines {
                        failed: &[(line, Uuid::nil())],
                        ..Default::default()
                    }
                )
                .await,
            Err(FenceError::Storage)
        ));
        seqs.push(
            fence
                .insert_segment(
                    &w.f.store,
                    ws,
                    file_id,
                    SegmentLines {
                        failed: &[(line, execution)],
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    assert_eq!(seqs, [1, 2, 3]);
}

/// A stale runner is refused at admission: no execution, no reservation, no
/// provider call, and the current claim admits the same line.
#[dbtest::test]
async fn a_stale_claim_admits_no_line(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let principal = job.principal();
    let deployment = InferenceRepository::deployments(&w.f.store, &principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let admit = |fence: BatchFence| {
        let (store, principal, deployment) = (w.f.store.clone(), principal, deployment.clone());
        let envelope = job.execution_id;
        async move {
            let id = Uuid::new_v4();
            admit_line(
                &store,
                LineContext { envelope, fence },
                &ExecutionStart {
                    id,
                    root_request_id: id,
                    attempt_number: 1,
                    principal,
                    deployment_id: deployment.id,
                    provider: deployment.provider.clone(),
                    model: "company/smart".into(),
                    streamed: false,
                    upstream_model: Some(deployment.upstream_model.clone()),
                    client: Default::default(),
                },
                &WorkloadAdmission {
                    kind: crate::inference::types::WorkloadKind::Generation,
                    output: OutputReservation::Requested(Some(10)),
                    unit_ceilings: Default::default(),
                },
                60,
                &deployment,
                None,
                50,
                false,
                None,
            )
            .await
        }
    };
    assert_eq!(admit(old).await.unwrap_err(), InferenceError::Configuration);
    let lines = |w: &World| {
        let (store, job) = (w.f.store.clone(), job.id);
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM inference_executions WHERE batch_job_id=$1",
            )
            .bind(job)
            .fetch_one(&store.pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(lines(&w).await, 0);
    admit(new).await.unwrap();
    assert_eq!(lines(&w).await, 1);
    consistent(&w.f).await;
}

/// A process that predates the epoch claims with the runner id only (its
/// statement, verbatim): the claim it replaces is fenced all the same, by the
/// runner id, and the epoch does not move.
#[dbtest::test]
async fn a_claim_by_a_process_without_the_epoch_still_fences_the_old_claim(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let ours = claim(&w, &job).await;
    lapse(&w, &job).await;
    let old = Uuid::new_v4();
    let claimed = sqlx::query("UPDATE async_jobs SET runner_id=$1,runner_lease_until=clock_timestamp()+make_interval(secs=>$2) WHERE id IN(SELECT id FROM async_jobs WHERE batch_mode='gateway' AND settled_at IS NULL AND (runner_lease_until IS NULL OR runner_lease_until<clock_timestamp()) ORDER BY created_at,id LIMIT $3 FOR UPDATE SKIP LOCKED)")
        .bind(old)
        .bind(60.0_f64)
        .bind(1_i64)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(claimed.rows_affected(), 1);
    assert_eq!(w.reload(&job).await.request_total, job.request_total);
    let epoch: i64 = sqlx::query_scalar("SELECT runner_epoch FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        epoch, ours.epoch,
        "the older statement does not know the epoch"
    );
    assert_eq!(ours.renew(&w.f.store, 60.0).await.unwrap(), Renewed::Fenced);
    assert!(matches!(
        ours.claim_line(&w.f.store, job.workspace_id, 0, Uuid::new_v4())
            .await,
        Err(FenceError::Stale)
    ));
}

/// The core of the fence: a claim skips a batch while a runner write is in
/// flight and takes it after, and then the earlier claim is refused.
async fn claim_skips_a_write_in_flight(pool: PgPool, hold: crate::jobs::fence::Hold) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    lapse(&w, &job).await;
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(fence.hold(&mut tx, hold).await.unwrap());
    let skipped = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap();
    assert!(
        skipped.is_empty(),
        "the claim waited for or overtook a write"
    );
    // A write that only reads and inserts does not queue the owner's own
    // renewal (`KEY SHARE` and the renewal's `NO KEY UPDATE` do not conflict).
    if hold == crate::jobs::fence::Hold::Rows {
        assert_eq!(
            fence.renew(&w.f.store, 0.001).await.unwrap(),
            Renewed::Held { cancel: false }
        );
    }
    tx.commit().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let taken = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(
        !fence.hold(&mut tx, hold).await.unwrap(),
        "the old claim still holds"
    );
}
#[dbtest::test]
async fn a_claim_skips_a_batch_with_a_row_hold_in_flight(pool: PgPool) {
    claim_skips_a_write_in_flight(pool, crate::jobs::fence::Hold::Rows).await;
}
#[dbtest::test]
async fn a_claim_skips_a_batch_with_an_update_hold_in_flight(pool: PgPool) {
    claim_skips_a_write_in_flight(pool, crate::jobs::fence::Hold::Update).await;
}

/// A fenced transaction that stops talking is ended by the server, which
/// releases the batch for a claim.
#[dbtest::test]
async fn a_fenced_transaction_carries_an_idle_timeout(pool: PgPool) {
    use crate::jobs::fence::{Hold, IDLE_IN_TRANSACTION};
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    for hold in [Hold::Rows, Hold::Update] {
        let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
        assert!(fence.hold(&mut tx, hold).await.unwrap());
        let timeout: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(timeout, IDLE_IN_TRANSACTION);
        tx.rollback().await.unwrap();
    }
    // The server enforces it: a held row is released when the session sits idle.
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(fence.hold(&mut tx, Hold::Update).await.unwrap());
    sqlx::query("SET LOCAL idle_in_transaction_session_timeout='200ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(sqlx::query("SELECT 1").execute(&mut *tx).await.is_err());
    lapse(&w, &job).await;
    assert_eq!(
        claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
            .await
            .unwrap()
            .len(),
        1
    );
}

/// A run that loses the batch between creating its result files and recording
/// the batch finished removes those files; the owner finalises alone.
#[dbtest::test]
async fn a_takeover_between_the_result_files_and_finishing_leaves_no_duplicate_files(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    w.jobs.finalize_hold.arm();
    let old = start(&w);
    w.jobs.finalize_hold.reached().await;
    // The old runner still renews while its finalization is held.
    assert!(
        start_replacement(&w, &job, w.jobs.clone())
            .await
            .unwrap()
            .unwrap()
    );
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    let files = live_files(&w).await;

    w.jobs.finalize_hold.release();
    assert!(matches!(old.await.unwrap(), Ok(true)));
    assert_eq!(
        live_files(&w).await,
        files - 1,
        "the old run's output file was removed"
    );
    let after = w.reload(&job).await;
    assert_eq!(after.output_file_id, done.output_file_id);
    assert!(!file_deleted(&w.f, after.output_file_id).await);
    assert_eq!(
        w.read(w.f.principal.workspace_id, after.output_file_id)
            .await
            .len(),
        1
    );
    assert_eq!(
        (after.request_completed, after.request_failed),
        (Some(1), Some(0))
    );
    consistent(&w.f).await;
}

/// Advance committed fixtures across the grace period without a wall-clock wait.
async fn age_result_files(w: &World, ids: &[Uuid]) {
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    sqlx::query("ALTER TABLE stored_files DISABLE TRIGGER stored_files_guard")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE stored_files SET created_at=now()-interval '2 days',committed_at=now()-interval '2 days' WHERE id=ANY($1)")
        .bind(ids)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE stored_files ENABLE TRIGGER stored_files_guard")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Losing the process after writing both files leaves their objects and rows,
/// with no job reference. Recovery must work even with batch retention off.
#[dbtest::test]
async fn crashed_batch_finalization_outputs_are_reclaimed_without_retention(pool: PgPool) {
    use crate::filestore::sweep::sweep_once;

    let (w, job) = batch(pool, &[("ok", "plain"), ("err", "fail")]).await;
    let db = &w.f.store;
    let runtime = w.files.runtime();
    let ws = job.workspace_id;
    let other_ws = w.f.team.workspace_id;
    let other_file = w
        .upload(&w.f.team, &[chat_line("other", "company/smart", "plain")])
        .await;
    let other_bytes = w
        .files
        .workspace_storage(other_ws)
        .await
        .unwrap()
        .used_bytes;
    w.jobs.finalize_hold.arm();
    let old = start(&w);
    w.jobs.finalize_hold.reached().await;
    let orphans: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT id,object_key,size_bytes FROM stored_files WHERE api_purpose='batch_output' AND workspace_id=$1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(ws)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(orphans.len(), 2);
    let orphan_ids: Vec<_> = orphans.iter().map(|(id, _, _)| *id).collect();
    // A long write may have started before the grace cutoff. Its freshly
    // committed result still belongs to the live producer.
    sqlx::query("UPDATE stored_files SET created_at=now()-interval '2 days' WHERE id=ANY($1)")
        .bind(&orphan_ids)
        .execute(&db.pool)
        .await
        .unwrap();
    let open = w.reload(&job).await;
    assert_eq!((open.output_file_id, open.error_file_id), (None, None));
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);
    old.abort();
    assert!(old.await.unwrap_err().is_cancelled());
    lapse(&w, &job).await;
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let done = w.reload(&job).await;
    assert!(done.settled_at.is_some());
    let cost = crate::governance::batch::batch_cost(db, job.id)
        .await
        .unwrap();
    let stored = w.files.workspace_storage(ws).await.unwrap().used_bytes;

    // The normal schema requires retention. Force NULL only in this
    // disposable fixture to exercise cleanup independently of that policy.
    let mut tx = crate::db::begin(&db.pool).await.unwrap();
    sqlx::query(
        "ALTER TABLE installation_settings ALTER COLUMN file_batch_retention_days DROP NOT NULL",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE installation_settings SET file_batch_retention_days=NULL")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut aged = orphan_ids;
    aged.extend([done.output_file_id.unwrap(), done.error_file_id.unwrap()]);
    age_result_files(&w, &aged).await;

    let user = w.f.principal.user_id.unwrap();
    let hold: Uuid = sqlx::query_scalar(
        "INSERT INTO legal_holds(id,user_id,reference) VALUES(gen_random_uuid(),$1,'batch-cleanup-hold') RETURNING id",
    )
    .bind(user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);
    sqlx::query("UPDATE legal_holds SET released_at=now(),released_by_user_id=$2 WHERE id=$1")
        .bind(hold)
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();

    let (a, b) = tokio::join!(sweep_once(db, runtime, 100), sweep_once(db, runtime, 100));
    assert_eq!(a.unwrap().deleted + b.unwrap().deleted, 2);
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);
    for (id, key, _) in &orphans {
        assert!(file_deleted(&w.f, Some(*id)).await);
        let key = crate::filestore::ObjectKey::parse(key).unwrap();
        assert!(runtime.store().unwrap().head(&key).await.unwrap().is_none());
    }
    let reclaimed = orphans.iter().map(|(_, _, size)| *size as u64).sum::<u64>();
    assert_eq!(
        w.files.workspace_storage(ws).await.unwrap().used_bytes,
        stored - reclaimed
    );
    assert_eq!(w.read(ws, done.output_file_id).await.len(), 1);
    assert_eq!(w.read(ws, done.error_file_id).await.len(), 1);
    let other_id = crate::filestore::files::parse_public_id(&other_file).unwrap();
    assert!(
        w.files
            .get(other_id, Some(other_ws))
            .await
            .unwrap()
            .is_some()
    );
    assert!(w.files.get(other_id, Some(ws)).await.unwrap().is_none());
    assert!(
        w.files
            .get(done.output_file_id.unwrap(), Some(other_ws))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        w.files
            .workspace_storage(other_ws)
            .await
            .unwrap()
            .used_bytes,
        other_bytes
    );
    assert_eq!(w.openai.executed().len(), 2);
    assert_eq!(
        crate::governance::batch::batch_cost(db, job.id)
            .await
            .unwrap(),
        cost
    );
    consistent(&w.f).await;
}

/// A rekey can hold a result lock across I/O. The runner must give up its
/// claim while the file remains locked, so a replacement can finish.
#[dbtest::test]
async fn rekey_locked_result_does_not_delay_batch_takeover(pool: PgPool) {
    let (w, job) = batch(pool, &[("ok", "plain"), ("err", "fail")]).await;
    let db = &w.f.store;
    w.jobs.finalize_hold.arm();
    let old = start(&w);
    w.jobs.finalize_hold.reached().await;

    let mut rekey = crate::db::begin(&db.pool).await.unwrap();
    let locked: Uuid = sqlx::query_scalar(
        "SELECT id FROM stored_files WHERE api_purpose='batch_output'
         ORDER BY id DESC LIMIT 1 FOR NO KEY UPDATE",
    )
    .fetch_one(&mut *rekey)
    .await
    .unwrap();
    w.jobs.finalize_hold.release();
    let error = tokio::time::timeout(Duration::from_secs(3), old)
        .await
        .expect("runner waited for the rekey lock")
        .unwrap();
    assert!(error.is_err());
    let open = w.reload(&job).await;
    assert_eq!(open.state, "in_progress");
    assert!(open.output_file_id.is_none() && open.error_file_id.is_none());

    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            Runner::new(w.jobs.clone()).run_next()
        )
        .await
        .expect("takeover waited for the old file lock")
        .unwrap()
    );
    let done = w.reload(&job).await;
    assert!(done.settled_at.is_some());
    assert_ne!(done.output_file_id, Some(locked));
    assert_ne!(done.error_file_id, Some(locked));
    assert_eq!(w.read(job.workspace_id, done.output_file_id).await.len(), 1);
    assert_eq!(w.read(job.workspace_id, done.error_file_id).await.len(), 1);
    rekey.rollback().await.unwrap();
    assert_eq!(w.openai.executed().len(), 2);
    consistent(&w.f).await;
}

/// Both lock orders are safe: a result being linked is skipped, and an
/// outstanding delete claim cannot be linked even before the object is gone.
#[dbtest::test]
async fn finalization_and_orphan_cleanup_exclude_each_other(pool: PgPool) {
    use crate::filestore::sweep::sweep_once;

    let (w, job) = batch(pool, &[("ok", "plain")]).await;
    let db = &w.f.store;
    let runtime = w.files.runtime();
    let fence = claim(&w, &job).await;
    let result_file = |internal, ws, key| {
        w.files.create(
            crate::jobs::io::batch_file(internal, ws, key, None, "results.jsonl".into()),
            Box::pin(futures_util::stream::once(async {
                Ok(axum::body::Bytes::from_static(b"{}\n"))
            })),
        )
    };
    let file = result_file(false, job.workspace_id, job.api_key_id)
        .await
        .unwrap();
    let other = result_file(false, w.f.team.workspace_id, w.f.team.key_id)
        .await
        .unwrap();
    let finish = |output, error_file| store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: Some(output),
        error_file,
        counts: None,
    };
    assert!(matches!(
        fence
            .finish_batch(db, &finish(file.id, Some(other.id)))
            .await,
        Err(FenceError::Storage)
    ));
    assert!(w.reload(&job).await.output_file_id.is_none());
    assert!(!file_deleted(&w.f, Some(other.id)).await);

    let internal = result_file(true, job.workspace_id, job.api_key_id)
        .await
        .unwrap();
    age_result_files(&w, &[file.id, internal.id]).await;
    let mut tx = crate::db::begin(&db.pool).await.unwrap();
    fence
        .hold_counted(&mut tx, crate::jobs::fence::Hold::Update)
        .await
        .unwrap();
    store::finish_batch_tx(&mut tx, job.id, &finish(file.id, None))
        .await
        .unwrap();
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);

    // Capture the old view of the references before the finalizer commits.
    // The claim must recheck after the file lock changes hands.
    let mut sweep = crate::db::begin(&db.pool).await.unwrap();
    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM stored_files f WHERE f.id=$1
         AND NOT EXISTS(SELECT 1 FROM async_jobs j WHERE j.output_file_id=f.id)",
    )
    .bind(file.id)
    .fetch_all(&mut *sweep)
    .await
    .unwrap();
    assert_eq!(candidates, [file.id]);
    tx.commit().await.unwrap();
    sqlx::query("SELECT id FROM stored_files WHERE id=$1 FOR UPDATE")
        .bind(file.id)
        .execute(&mut *sweep)
        .await
        .unwrap();
    assert!(
        crate::filestore::sweep::claim_locked_candidates(&mut sweep, &candidates)
            .await
            .unwrap()
            .is_empty()
    );
    sweep.commit().await.unwrap();
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);
    assert!(!file_deleted(&w.f, Some(file.id)).await);
    assert!(!file_deleted(&w.f, Some(internal.id)).await);

    let input = public_id(job.input_file_id.unwrap());
    let retry = w
        .create(w.f.principal, &input, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let orphan = result_file(false, job.workspace_id, job.api_key_id)
        .await
        .unwrap();
    age_result_files(&w, &[orphan.id]).await;
    let failed = sweep_once(db, &FileStoreRuntime::off(), 100).await.unwrap();
    assert_eq!((failed.claimed, failed.deleted, failed.failed), (1, 0, 1));
    assert!(
        runtime
            .store()
            .unwrap()
            .head(&orphan.key)
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        store::finish_batch(db, retry.id, &finish(orphan.id, None)).await,
        Err(sqlx::Error::RowNotFound)
    ));
    assert!(w.reload(&retry).await.output_file_id.is_none());
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().claimed, 0);
    sqlx::query(
        "UPDATE stored_files SET last_delete_attempt_at=now()-interval '6 minutes' WHERE id=$1",
    )
    .bind(orphan.id)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().deleted, 1);
    assert!(
        runtime
            .store()
            .unwrap()
            .head(&orphan.key)
            .await
            .unwrap()
            .is_none()
    );
    let replacement = result_file(false, job.workspace_id, job.api_key_id)
        .await
        .unwrap();
    let done = store::finish_batch(db, retry.id, &finish(replacement.id, None))
        .await
        .unwrap();
    assert_eq!(done.output_file_id, Some(replacement.id));

    // Explicit expiry still applies to linked results.
    sqlx::query("UPDATE stored_files SET expires_at=now() WHERE id=$1")
        .bind(file.id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(sweep_once(db, runtime, 100).await.unwrap().deleted, 1);
    assert!(file_deleted(&w.f, Some(file.id)).await);
}

/// The next owner can read a completed line's segment while it interrupts
/// the line whose provider call is still in flight.
#[dbtest::test]
async fn a_committed_result_survives_takeover_with_another_line_in_flight(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "hold-b")]).await;
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,updated_by) VALUES($1,1,$2)")
        .bind(w.f.deployment).bind(w.f.owner).execute(&w.f.store.pool).await.unwrap();
    let held = w.openai.gate("hold-b");
    // Line `a`'s segment stays open while `b` is planned (at most `REACH`) and
    // the state below is read, then flushes on its age.
    let age = REACH + Duration::from_secs(5);
    let old = start_with(flushing_after(&w, age));
    tokio::time::timeout(REACH, held.entered.notified())
        .await
        .expect("the second provider call needs no segment flush to start");
    let line = || {
        sqlx::query_as::<_, (String, Option<i32>, bool)>(
            "SELECT state,segment,awaiting_segment FROM batch_lines WHERE job_id=$1 AND line_no=0",
        )
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
    };
    assert_eq!(line().await.unwrap(), ("running".into(), None, true));
    assert_eq!(w.reload(&job).await.request_completed, Some(0));
    assert_eq!(
        crate::jobs::schedule::Scheduler::new(w.f.store.clone(), None)
            .running_lines(w.f.deployment)
            .await
            .unwrap(),
        1
    );
    tokio::time::timeout(age + Duration::from_secs(3), async {
        loop {
            if line().await.unwrap() == ("succeeded".into(), Some(0), false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the oldest outcome must flush while another provider is blocked");
    assert_eq!(segments(&w, &job).await, 1);
    assert!(
        start_replacement(&w, &job, w.jobs.clone())
            .await
            .unwrap()
            .unwrap()
    );
    let done = w.reload(&job).await;
    let output = w
        .read(w.f.principal.workspace_id, done.output_file_id)
        .await;
    let errors = w.read(w.f.principal.workspace_id, done.error_file_id).await;
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["custom_id"], "a");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["custom_id"], "b");
    assert_eq!(errors[0]["error"]["code"], "result_unavailable");
    assert_eq!(
        (
            done.request_total,
            done.request_completed,
            done.request_failed
        ),
        (Some(2), Some(1), Some(1))
    );
    held.release.notify_one();
    assert!(matches!(old.await.unwrap(), Ok(true)));
    assert_eq!(
        segments(&w, &job).await,
        0,
        "finalization deleted the segment"
    );
    consistent(&w.f).await;
}

/// Every statement the scheduler and the job row accept from a claim that
/// ended is refused or does nothing; the current claim's go through.
#[dbtest::test]
async fn scheduler_and_job_row_writes_follow_the_claim(pool: PgPool) {
    use crate::jobs::schedule::{Claim, Pause, Scheduler, WaitRow};
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain")]).await;
    let s = &w.f.store;
    let scheduler = Scheduler::new(s.clone(), None);
    let route = w.f.deployment;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let ws = job.workspace_id;
    let row = |n| WaitRow {
        deployment: route,
        waiting_lines: n,
        reason: Some(Pause::Concurrency),
        ready: true,
    };
    // Ended claim.
    assert!(stale(
        scheduler.claim(&old, ws, 0, Uuid::new_v4(), route, 2).await
    ));
    assert!(stale(
        scheduler
            .claim_retry(&old, 0, 1, Uuid::new_v4(), Some((route, 2)))
            .await
    ));
    assert!(stale(
        scheduler.publish_waits(&old, ws, &[row(1)], true).await
    ));
    assert!(stale(scheduler.clear_waits(&old).await));
    old.mark_running(s).await.unwrap();
    old.mark_settled(s).await.unwrap();
    assert_eq!(
        count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await,
        0
    );
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        0
    );
    assert_eq!(
        w.reload(&job).await.state,
        "queued",
        "the ended claim marked it running"
    );
    // Current claim.
    assert_eq!(
        scheduler
            .claim(&new, ws, 0, Uuid::new_v4(), route, 2)
            .await
            .unwrap(),
        Claim::Claimed
    );
    scheduler
        .publish_waits(&new, ws, &[row(3)], true)
        .await
        .unwrap();
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        1
    );
    assert_eq!(
        scheduler
            .claim_retry(&new, 0, 1, Uuid::new_v4(), Some((route, 2)))
            .await
            .unwrap(),
        RetryClaim::LineChanged,
        "a running line is not a failed one: nothing to retry"
    );
    new.mark_running(s).await.unwrap();
    assert_eq!(w.reload(&job).await.state, "in_progress");
    new.progress(s, 1, 2).await.unwrap();
    new.set_status(s, "finalizing").await.unwrap();
    let row = w.reload(&job).await;
    assert_eq!(
        (
            row.request_completed,
            row.request_failed,
            row.upstream_status.as_deref()
        ),
        (Some(1), Some(2), Some("finalizing"))
    );
    scheduler.clear_waits(&new).await.unwrap();
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        0
    );
    // Settling follows finishing.
    old.mark_settled(s).await.unwrap();
    let done = crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: None,
        error_file: None,
        counts: None,
    };
    let crate::jobs::fence::Publication::Linked(finished) =
        new.finish_batch(s, &done).await.unwrap()
    else {
        panic!("expected published results");
    };
    assert_eq!(finished.state, "completed");
    old.mark_settled(s).await.unwrap();
    assert!(
        w.reload(&job).await.settled_at.is_none(),
        "the ended claim settled it"
    );
    new.mark_settled(s).await.unwrap();
    assert!(w.reload(&job).await.settled_at.is_some());
    // Renewal reports a settled batch, not a lost claim; release is fenced.
    assert_eq!(new.renew(s, 60.0).await.unwrap(), Renewed::Settled);
    new.release(s).await.unwrap();
    assert!(stale(old.release(s).await));
}

/// The first failed renewal makes the writer store what it holds, before a
/// takeover can refuse it; a writer whose claim ended stores nothing.
#[dbtest::test]
async fn a_failed_renewal_stores_the_buffered_results_at_once(pool: PgPool) {
    use crate::jobs::runner::SegmentProbe;
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain")]).await;
    let old = claim(&w, &job).await;
    let executions = [Uuid::new_v4(), Uuid::new_v4()];
    for (line, execution) in (0..2).zip(executions) {
        assert!(
            old.claim_line(&w.f.store, job.workspace_id, line, execution)
                .await
                .unwrap()
        );
        assert!(
            old.fail_line(&w.f.store, line, execution, Some(500), Some("failed"), true)
                .await
                .unwrap()
        );
    }
    let probe = SegmentProbe::start(w.f.store.clone(), w.files.clone(), job.clone(), old);
    probe.finished(0, executions[0]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        segments(&w, &job).await,
        0,
        "nothing flushes before the age or size threshold"
    );
    probe.doubt();
    for _ in 0..50 {
        if segments(&w, &job).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(segments(&w, &job).await, 1, "the doubt flushed the buffer");
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT segment FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(0));
    // The batch is taken over: the next flush is refused and stores nothing.
    let _new = claim(&w, &job).await;
    probe.finished(1, executions[1]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    probe.doubt();
    for _ in 0..50 {
        if probe.fenced() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(probe.fenced());
    probe.close().await;
    assert_eq!(segments(&w, &job).await, 1);
}

/// A run that only doubts its lease (soft loss) never ends the batch: it
/// finishes its in-flight lines, releases, and the batch stays open with the
/// unstarted lines pending for the next claim.
#[dbtest::test]
async fn a_soft_lost_run_leaves_the_batch_open_and_a_second_runner_completes_it(pool: PgPool) {
    let (w, job) = batch(
        pool,
        &[("a", "plain-a"), ("b", "plain-b"), ("c", "plain-c")],
    )
    .await;
    w.jobs
        .soft_loss
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(start(&w).await.unwrap(), Ok(true)));
    let open = w.reload(&job).await;
    assert_eq!(open.state, "in_progress", "a soft-lost run ended the batch");
    assert_eq!(
        (open.output_file_id, open.error_file_id, open.settled_at),
        (None, None, None)
    );
    assert_eq!(envelope(&w, &job).await, "pending");
    let started = count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await;
    assert!(
        (1..3).contains(&started),
        "unstarted lines stay pending: {started}"
    );
    let lease: Option<gateway_datetime_compat::UtcInstant> =
        sqlx::query_scalar("SELECT runner_lease_until FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert!(lease.is_none(), "the run gave the batch up");

    // The next claim runs the rest and ends the batch.
    assert!(matches!(start(&w).await.unwrap(), Ok(true)));
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(3), Some(0))
    );
    let out = w
        .read(w.f.principal.workspace_id, done.output_file_id)
        .await;
    assert_eq!(out.len(), 3);
    assert_eq!(w.openai.executed().len(), 3, "every line exactly once");
    consistent(&w.f).await;
}

/// A missing file and a stale fence are detected before COMMIT.
#[dbtest::test]
async fn finishing_distinguishes_rejection_and_stale_claim(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let done = |file: Option<Uuid>| crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: file,
        error_file: None,
        counts: None,
    };
    assert!(matches!(
        new.finish_batch(&w.f.store, &done(Some(Uuid::new_v4())))
            .await,
        Err(FenceError::Storage)
    ));
    assert!(matches!(
        old.finish_batch(&w.f.store, &done(None)).await,
        Err(FenceError::Stale)
    ));
    assert_eq!(w.reload(&job).await.state, "queued");
}

#[dbtest::test]
async fn unacknowledged_finish_leaves_actual_results_for_sweep(pool: PgPool) {
    let (w, job) = batch(pool, &[("ok", "plain"), ("err", "fail")]).await;
    let db = &w.f.store;
    let runtime = w.files.runtime();
    w.jobs.finalize_hold.arm();
    let old = start(&w);
    w.jobs.finalize_hold.reached().await;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM stored_files WHERE api_purpose='batch_output' AND committed_at IS NOT NULL",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(ids.len(), 2);
    sqlx::raw_sql(
        "CREATE FUNCTION reject_finish_at_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'fixture refused batch commit'; END $$;
         CREATE CONSTRAINT TRIGGER refuse_finish_at_commit AFTER UPDATE ON async_jobs
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
         WHEN (OLD.state='in_progress' AND NEW.state='completed')
         EXECUTE FUNCTION reject_finish_at_commit()",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    w.jobs.finalize_hold.release();
    assert!(old.await.unwrap().is_err());
    let open = w.reload(&job).await;
    assert_eq!(open.state, "in_progress");
    assert!(open.output_file_id.is_none() && open.error_file_id.is_none());
    for &id in &ids {
        assert_eq!(w.read(job.workspace_id, Some(id)).await.len(), 1);
    }

    sqlx::query("DROP TRIGGER refuse_finish_at_commit ON async_jobs")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let done = w.reload(&job).await;
    assert!(done.settled_at.is_some());
    let output = done.output_file_id.unwrap();
    let error = done.error_file_id.unwrap();
    assert!(!ids.contains(&output) && !ids.contains(&error));
    let mut aged = ids.clone();
    aged.extend([output, error]);
    age_result_files(&w, &aged).await;
    let swept = crate::filestore::sweep::sweep_once(db, runtime, 200)
        .await
        .unwrap();
    assert_eq!((swept.claimed, swept.deleted, swept.failed), (2, 2, 0));
    for id in ids {
        assert!(file_deleted(&w.f, Some(id)).await);
    }
    assert_eq!(w.read(job.workspace_id, Some(output)).await.len(), 1);
    assert_eq!(w.read(job.workspace_id, Some(error)).await.len(), 1);
    assert_eq!(w.openai.executed().len(), 2);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn cancelling_flushes_queued_and_in_flight_answers_without_running_the_tail(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "hold-b"), ("c", "never")]).await;
    let held = w.openai.gate("hold-b");
    // The answers reach the result only through the cancel: no segment
    // flushes on its age while `b` is planned (at most `REACH`), the runner
    // observes the cancel and the run finishes.
    let (observe, finish) = (Duration::from_secs(20), Duration::from_secs(5));
    let age = REACH + observe + finish + Duration::from_secs(5);
    let running = start_with(flushing_after(&w, age));
    tokio::time::timeout(REACH, held.entered.notified())
        .await
        .unwrap();
    assert_eq!(w.reload(&job).await.request_completed, Some(0));
    let lease = || {
        sqlx::query_scalar::<_, gateway_datetime_compat::UtcInstant>(
            "SELECT runner_lease_until FROM async_jobs WHERE id=$1",
        )
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
    };
    let initial = lease().await.unwrap();
    w.jobs
        .cancel_batch(&w.f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    // Let the active runner observe cancel before its current provider call returns.
    tokio::time::timeout(observe, async {
        while lease().await.unwrap() == initial {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    held.release.notify_one();
    assert!(
        tokio::time::timeout(finish, running)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let done = w.reload(&job).await;
    assert_eq!(done.state, "cancelled");
    assert_eq!(w.openai.executed(), ["plain", "hold-b"]);
    let ws = w.f.principal.workspace_id;
    let output = w.read(ws, done.output_file_id).await;
    assert_eq!(
        output
            .iter()
            .map(|line| line["custom_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let errors = w.read(ws, done.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["custom_id"], "c");
    assert_eq!(errors[0]["error"]["code"], "batch_cancelled");
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_segment_commit_error_keeps_the_file_and_recovery_does_not_replay(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "one"), ("b", "two")]).await;
    let db = &w.f.store.pool;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_segment_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'segment commit refused' USING ERRCODE='23514'; END $$;
        CREATE CONSTRAINT TRIGGER fail_segment_commit AFTER INSERT ON batch_segments
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_segment_commit()",
    )
    .execute(db)
    .await
    .unwrap();
    // The whole run, provider calls included: a planning failure costs one REPLAN.
    assert!(
        tokio::time::timeout(REACH, start(&w))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let waiting = w.reload(&job).await;
    assert_eq!(waiting.state, "in_progress");
    assert_eq!(waiting.request_completed, Some(0));
    assert_eq!(segments(&w, &job).await, 0);
    let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE workspace_id=$1
        AND filename LIKE 'batch_%_part_%' AND deleted_at IS NULL AND committed_at IS NOT NULL AND api_purpose IS NULL")
        .bind(job.workspace_id).fetch_one(db).await.unwrap();
    assert_eq!(
        retained, 1,
        "an unacknowledged commit leaves one private object"
    );
    sqlx::query("DROP TRIGGER fail_segment_commit ON batch_segments")
        .execute(db)
        .await
        .unwrap();
    assert!(start(&w).await.unwrap().unwrap());
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(0), Some(2))
    );
    assert_eq!(w.openai.executed(), ["one", "two"]);
    let errors = w.read(job.workspace_id, done.error_file_id).await;
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e["error"]["code"] == "result_unavailable")
    );
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (30, 0));
    consistent(&w.f).await;
}

#[dbtest::test]
async fn backoff_retry_legacy_refuses_persisted_cancel(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    let store = &w.f.store;
    let first = Uuid::new_v4();
    assert!(
        fence
            .claim_line(store, job.workspace_id, 0, first)
            .await
            .unwrap()
    );
    assert!(
        fence
            .fail_line(
                store,
                0,
                first,
                Some(502),
                Some("upstream_unavailable"),
                false
            )
            .await
            .unwrap()
    );
    crate::jobs::store::mark_cancel_requested(store, job.id)
        .await
        .unwrap();
    assert_eq!(
        Scheduler::new(store.clone(), None)
            .claim_retry(&fence, 0, 1, Uuid::new_v4(), None)
            .await
            .unwrap(),
        RetryClaim::Cancelled
    );
}
