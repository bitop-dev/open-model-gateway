//! A gateway-run batch on a drained route (#49 D3). A batch whose model has
//! only drained routes is refused at creation, as during a cooldown, with
//! the retryable `model_temporarily_unavailable` (never `model_not_found`);
//! a line of a batch created before the drain, not yet dispatched, waits as
//! for a route cooling down: nothing is sent, nothing is claimed, no retry is
//! spent.
//! A line already in flight when the drain is set sends nothing more: its
//! retry finds the route drained and the line fails once its retries are
//! spent, as for an open circuit.
use super::fence::{REACH, start};
use super::*;

async fn drain(w: &World, until: &str) {
    sqlx::query(&format!("UPDATE deployments SET drain_reason='maintenance', drain_until={until}, drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1"))
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.f.store.caches.forget_health(w.f.deployment);
}

#[dbtest::test]
async fn a_line_waits_for_a_drain_and_a_new_batch_is_told_to_retry(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "plain")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    drain(&w, "NULL").await;
    // A new batch for the drained model: temporarily unavailable, not missing.
    match w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    {
        Err(CreateError::Job(JobError::Inference(InferenceError::RouteDrained(None)))) => {}
        other => panic!("{:?}", other.map(|j| j.id)),
    }
    let running = start(&w);
    // Two dispatch passes and more: nothing sent, the line not claimed.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(w.openai.executed().is_empty());
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM batch_lines WHERE job_id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(claimed, 0, "a line waiting on a drain is not claimed");
    // The drain is cleared: the line is planned again (after REPLAN) and runs.
    sqlx::query("UPDATE deployments SET drain_reason=NULL,drain_until=NULL,drained_by=NULL,drained_at=NULL WHERE id=$1")
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.f.store.caches.forget_health(w.f.deployment);
    assert!(
        tokio::time::timeout(REACH, running)
            .await
            .expect("the line runs once the drain ends")
            .unwrap()
            .unwrap()
    );
    assert_eq!(w.openai.executed(), ["plain"]);
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(1), Some(0))
    );
}

#[dbtest::test]
async fn a_line_in_flight_when_its_route_is_drained_sends_nothing_more(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "fail")])
        .await;
    let job = w
        .create(
            w.f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_retries":"1"})),
        )
        .await
        .unwrap();
    let gate = w.openai.gate("fail");
    let running = start(&w);
    tokio::time::timeout(REACH, gate.entered.notified())
        .await
        .expect("the first attempt reaches the upstream");
    drain(&w, "NULL").await;
    gate.release.notify_one();
    assert!(
        tokio::time::timeout(REACH, running)
            .await
            .expect("the line ends: it does not wait on the drain")
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        w.openai.executed(),
        ["fail"],
        "nothing sent after the drain"
    );
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(0), Some(1))
    );
}

/// A native batch accepted before its route is drained is not submitted
/// while the drain applies: it stays queued, nothing reaches the provider,
/// and it is submitted at its first poll once the drain ends.
#[dbtest::test]
async fn a_native_batch_queued_before_a_drain_waits_to_be_submitted(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Native));
    drain(&w, "NULL").await;
    w.poll().await;
    assert!(
        w.openai.script.lock().unwrap().submitted.is_empty(),
        "nothing is submitted to a drained route"
    );
    let queued: (String, bool) =
        sqlx::query_as("SELECT state, submit_started_at IS NULL FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(queued, ("queued".into(), true));
    sqlx::query("UPDATE deployments SET drain_reason=NULL,drain_until=NULL,drained_by=NULL,drained_at=NULL WHERE id=$1")
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.poll().await;
    assert_eq!(w.openai.script.lock().unwrap().submitted.len(), 1);
}

/// A native batch accepted on a route, ready for its first poll.
async fn native(w: &World) -> JobRow {
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Native));
    job
}

/// Less than the batch's completion window is left before its poll
/// deadline (as hours later): the clock column moves, nothing waits.
async fn too_late(w: &World, job: &JobRow) {
    sqlx::query("UPDATE async_jobs SET poll_deadline_at=now()+make_interval(hours=>coalesce(completion_window_hours,24)::int-1) WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// The batch ended `failed` with `submission_expired` and nothing sent: its
/// reservation is settled at a known zero, with its ledger entry, its
/// execution reads failed, and the job is settled.
async fn ended_unsent(w: &World, job: &JobRow) {
    settled_unsent(w, job, "failed", Some("submission_expired"), "failed").await;
}

/// The batch ended `state` (`error`) with nothing sent: settled at a known
/// zero, its execution `execution`.
async fn settled_unsent(
    w: &World,
    job: &JobRow,
    state: &str,
    error: Option<&str>,
    execution: &str,
) {
    assert!(w.openai.script.lock().unwrap().submitted.is_empty());
    let (got, code, settled, started): (String, Option<String>, bool, bool) = sqlx::query_as(
        "SELECT state, error_code, settled_at IS NOT NULL, submit_started_at IS NOT NULL FROM async_jobs WHERE id=$1",
    )
    .bind(job.id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        (got.as_str(), code.as_deref(), settled, started),
        (state, error, true, false)
    );
    let (ran, ran_code): (String, Option<String>) =
        sqlx::query_as("SELECT state, error_code FROM inference_executions WHERE id=$1")
            .bind(job.execution_id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(ran, execution);
    if execution == "failed" {
        assert_eq!(ran_code.as_deref(), Some("submission_expired"));
    }
    let (reservation, _, actual, _) = reservation(&w.f, job.execution_id).await;
    assert_eq!((reservation.as_str(), actual), ("settled", Some(0)));
    let ledger: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1 AND kind<>'hold' ORDER BY kind",
    )
    .bind(job.execution_id)
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    assert!(
        !ledger.is_empty()
            && ledger
                .iter()
                .all(|(kind, amount)| kind != "unknown" && amount.unwrap_or(0) == 0),
        "{ledger:?}"
    );
}

/// Drained with no end: while its full completion window is still left the
/// batch waits; past that it ends unsent at zero, never queued for ever.
#[dbtest::test]
async fn a_native_batch_drained_past_its_window_ends_unsent_at_zero(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let job = native(&w).await;
    drain(&w, "NULL").await;
    w.poll().await;
    assert_eq!(w.reload(&job).await.state, "queued");
    too_late(&w, &job).await;
    w.poll().await;
    ended_unsent(&w, &job).await;
}

/// The drain ends, but too late for the batch's results to be collected in
/// its poll window: it is not submitted (no provider work nobody collects)
/// and ends unsent at zero.
#[dbtest::test]
async fn a_native_batch_whose_drain_ends_too_late_is_never_submitted(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let job = native(&w).await;
    drain(&w, "NULL").await;
    w.poll().await;
    sqlx::query("UPDATE deployments SET drain_reason=NULL,drain_until=NULL,drained_by=NULL,drained_at=NULL WHERE id=$1")
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    too_late(&w, &job).await;
    w.poll().await;
    ended_unsent(&w, &job).await;
}

/// Drained, then disabled, then the drain cleared: the route does not serve,
/// so the submission is not claimed (nothing spent, nothing interrupted); once
/// the window is gone the batch ends unsent at zero.
#[dbtest::test]
async fn a_native_batch_on_a_disabled_route_is_not_claimed_and_ends_unsent(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let job = native(&w).await;
    drain(&w, "NULL").await;
    sqlx::query("UPDATE deployments SET enabled=false, drain_reason=NULL,drain_until=NULL,drained_by=NULL,drained_at=NULL WHERE id=$1")
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.poll().await;
    let started: bool =
        sqlx::query_scalar("SELECT submit_started_at IS NOT NULL FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert!(
        !started,
        "a route that does not serve is never claimed for submission"
    );
    too_late(&w, &job).await;
    w.poll().await;
    ended_unsent(&w, &job).await;
}

/// Past its poll deadline (the poller was down), an unsubmitted native batch
/// is still claimed, and ends unsent at zero instead of staying queued.
#[dbtest::test]
async fn an_unsubmitted_native_batch_past_its_deadline_still_ends(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let job = native(&w).await;
    drain(&w, "NULL").await;
    sqlx::query("UPDATE async_jobs SET poll_deadline_at=now()-interval '1 minute' WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.poll().await;
    ended_unsent(&w, &job).await;
}

/// Move the batch's poll deadline to `offset` (SQL interval text) after its
/// completion window plus the 1-hour collection margin from now.
async fn deadline_at_margin(w: &World, job: &JobRow, offset: &str) {
    sqlx::query(&format!("UPDATE async_jobs SET poll_deadline_at=now()+make_interval(hours=>coalesce(completion_window_hours,24)::int+1)+interval '{offset}' WHERE id=$1"))
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// The submit window's edge: with its completion window and the collection
/// margin still left (five minutes to spare) the batch is submitted; with
/// five minutes fewer it ends unsent.
#[dbtest::test]
async fn a_native_batch_is_submitted_only_with_its_window_and_margin_left(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let in_time = native(&w).await;
    deadline_at_margin(&w, &in_time, "5 minutes").await;
    w.poll().await;
    assert_eq!(
        w.openai.script.lock().unwrap().submitted.len(),
        1,
        "submitted in time"
    );
    w.openai.script.lock().unwrap().submitted.clear();
    let late = native(&w).await;
    deadline_at_margin(&w, &late, "-5 minutes").await;
    w.poll().await;
    ended_unsent(&w, &late).await;
}

/// The expiry committed but its zero settlement did not (the process
/// stopped in between): the next poll settles it at zero. The same for a
/// batch cancelled before submission whose settlement did not complete.
#[dbtest::test]
async fn an_unsent_batch_whose_settlement_did_not_complete_is_settled_at_its_next_poll(
    pool: PgPool,
) {
    let w = world(pool, true, false, 2).await;
    for (state, error, execution) in [
        ("failed", Some("submission_expired"), "failed"),
        ("cancelled", None, "cancelled"),
    ] {
        let job = native(&w).await;
        // What `expire_submission` (or the cancellation) commits, without
        // the settlement that follows it.
        sqlx::query("UPDATE async_jobs SET state=$2, upstream_status=$2, error_code=$3, completed_at=clock_timestamp(), cancel_requested_at=CASE WHEN $2='cancelled' THEN clock_timestamp() END WHERE id=$1")
            .bind(job.id)
            .bind(state)
            .bind(error)
            .execute(&w.f.store.pool)
            .await
            .unwrap();
        let (pending, _, _, _) = reservation(&w.f, job.execution_id).await;
        assert_eq!(pending, "pending");
        w.poll().await;
        settled_unsent(&w, &job, state, error, execution).await;
    }
}

/// A batch whose submission was claimed (it may have reached the provider)
/// and has no provider id is never ended `submission_expired` nor settled at
/// zero, in time or late, before or past its poll deadline: it is left to
/// `submission_interrupted`, which keeps its hold.
#[dbtest::test]
async fn a_claimed_submission_is_never_expired_or_settled_at_zero(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let job = native(&w).await;
    sqlx::query("UPDATE async_jobs SET submit_started_at=clock_timestamp() WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    for deadline in ["now()+interval '30 minutes'", "now()-interval '1 minute'"] {
        sqlx::query(&format!(
            "UPDATE async_jobs SET poll_deadline_at={deadline} WHERE id=$1"
        ))
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
        w.poll().await;
        let (state, error, settled): (String, Option<String>, bool) = sqlx::query_as(
            "SELECT state, error_code, settled_at IS NOT NULL FROM async_jobs WHERE id=$1",
        )
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            (state.as_str(), error, settled),
            ("queued", None, false),
            "{deadline}"
        );
        let (reservation, _, actual, _) = reservation(&w.f, job.execution_id).await;
        assert_eq!(
            (reservation.as_str(), actual),
            ("pending", None),
            "{deadline}"
        );
    }
    assert!(w.openai.script.lock().unwrap().submitted.is_empty());
}

/// A job's route is chosen as the routes are now: with health this replica
/// read before the first route was drained (planning still names it), the
/// job takes the second route; with every route drained it is temporarily
/// unavailable (503 `model_temporarily_unavailable`).
#[dbtest::test]
async fn a_job_route_drained_after_the_plan_is_passed_over(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let second = second_claude_route(&w).await;
    let first = w.claude_deployment;
    w.f.store.caches.versions.enable();
    let fresh = |id| {
        (
            id,
            crate::cache::HealthEntry {
                loaded: std::time::Instant::now() + std::time::Duration::from_secs(3600),
                open_until: None,
                consecutive_failures: 0,
                observed_age: Some(std::time::Duration::ZERO),
                probe_until: None,
                reopen_count: 0,
                drained: false,
                drain_until: None,
            },
        )
    };
    let select = || {
        w.jobs.select(
            &w.f.principal,
            "company/claude",
            ApiProtocol::ChatCompletions,
            Uuid::new_v4(),
            |_, _| true,
        )
    };
    assert_eq!(select().await.unwrap().0.id, first, "the first route leads");
    w.f.store
        .caches
        .store_health(vec![fresh(first), fresh(second)]);
    sqlx::query("UPDATE deployments SET drain_reason='maintenance', drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1")
        .bind(first)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(select().await.unwrap().0.id, second);
    w.f.store
        .caches
        .store_health(vec![fresh(first), fresh(second)]);
    sqlx::query("UPDATE deployments SET drain_reason='maintenance', drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1")
        .bind(second)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let error = select().await.err().unwrap();
    assert_eq!(error, InferenceError::RouteDrained(None));
    assert_eq!(error.code(), "model_temporarily_unavailable");
}

/// Past its poll deadline: a batch that ended unsent and whose settlement
/// did not complete is still claimed and settled at zero; a claimed
/// submission that never recorded a provider id is still claimed and, past
/// the grace, ends `submission_interrupted`, keeping its hold.
#[dbtest::test]
async fn past_the_deadline_an_unsent_end_settles_and_an_unrecorded_submission_is_interrupted(
    pool: PgPool,
) {
    let w = world(pool, true, false, 2).await;
    let unsent = native(&w).await;
    sqlx::query("UPDATE async_jobs SET state='failed', upstream_status='failed', error_code='submission_expired', completed_at=clock_timestamp(), poll_deadline_at=now()-interval '1 minute' WHERE id=$1")
        .bind(unsent.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let claimed = native(&w).await;
    sqlx::query("UPDATE async_jobs SET submit_started_at=now()-interval '2 hours', poll_deadline_at=now()-interval '1 minute' WHERE id=$1")
        .bind(claimed.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.poll().await;
    settled_unsent(&w, &unsent, "failed", Some("submission_expired"), "failed").await;
    let (state, error): (String, Option<String>) =
        sqlx::query_as("SELECT state, error_code FROM async_jobs WHERE id=$1")
            .bind(claimed.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("failed", Some("submission_interrupted"))
    );
    let (_, _, actual, _) = reservation(&w.f, claimed.execution_id).await;
    assert_ne!(
        actual,
        Some(0),
        "a submission that may have been sent is never settled at zero"
    );
    assert!(w.openai.script.lock().unwrap().submitted.is_empty());
}
