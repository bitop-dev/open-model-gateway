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
