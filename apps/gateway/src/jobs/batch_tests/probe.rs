//! A batch line on a half-open route (#49) runs only as its probe: while
//! another request holds the probe slot, the line waits as it would for a
//! route cooling down (nothing is sent, nothing is claimed, no retry is
//! spent), and once the slot is free it is the probe.
use super::fence::{REACH, batch, start};
use super::*;
use crate::routing::{ProbeClaim, claim_probe, release_probe};

#[dbtest::test]
async fn a_line_waits_for_a_probe_held_elsewhere_then_probes(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let db = &w.f.store.pool;
    let deployment = w.f.deployment;
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,60) ON CONFLICT (deployment_id) DO UPDATE SET failure_threshold=1,cooldown_seconds=60")
        .bind(deployment).execute(db).await.unwrap();
    crate::routing::record_result(&w.f.store, deployment, Some(InferenceError::Busy))
        .await
        .unwrap();
    sqlx::query("UPDATE deployment_health SET open_until=clock_timestamp() WHERE deployment_id=$1")
        .bind(deployment)
        .execute(db)
        .await
        .unwrap();
    let ProbeClaim::Claimed(elsewhere) = claim_probe(
        &w.f.store,
        deployment,
        Uuid::new_v4(),
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap() else {
        panic!("another request holds the probe")
    };
    let running = start(&w);
    // Two dispatch passes and more: nothing is sent and the line is not claimed.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(w.openai.executed().is_empty());
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM batch_lines WHERE job_id=$1")
        .bind(job.id)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(claimed, 0, "a line waiting for the probe is not claimed");
    // The slot is freed: the line is planned again (after REPLAN) and runs as the probe.
    release_probe(&w.f.store, deployment, elsewhere)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(REACH, running)
            .await
            .expect("the line runs once the slot is free")
            .unwrap()
            .unwrap()
    );
    assert_eq!(w.openai.executed(), ["plain"]);
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(1), Some(0))
    );
    let (open, reopens, held): (bool, i32, bool) = sqlx::query_as(
        "SELECT open_until IS NOT NULL, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(deployment)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(
        (open, reopens, held),
        (false, 0, false),
        "the line's success closed the circuit"
    );
}

/// A gateway-run batch of one `text` line with one retry on `company/smart`.
async fn retried(w: &World, text: &str) -> JobRow {
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", text)])
        .await;
    w.create(
        w.f.principal,
        &file,
        BatchEndpoint::ChatCompletions,
        Some(json!({"omg_retries":"1"})),
    )
    .await
    .unwrap()
}

/// The route's circuit is now as `open_until` sets it (one opening, failures
/// at the threshold): written directly, so no test waits on a cooldown.
async fn circuit_until(w: &World, open_until: &str) {
    sqlx::query(&format!(
        "INSERT INTO deployment_health(deployment_id,consecutive_failures,open_until,last_observed_at,reopen_count)
         VALUES($1,3,{open_until},clock_timestamp(),1)
         ON CONFLICT (deployment_id) DO UPDATE SET consecutive_failures=3,open_until=EXCLUDED.open_until,reopen_count=1"
    ))
    .bind(w.f.deployment)
    .execute(&w.f.store.pool)
    .await
    .unwrap();
}

/// The circuit (open, reopen count, probe held) of `w.f.deployment`.
async fn health_row(w: &World) -> (bool, i32, bool) {
    sqlx::query_as(
        "SELECT open_until IS NOT NULL, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(w.f.deployment)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap()
}

/// The line's first attempt is in flight when its route's circuit opens and
/// its cooldown passes (half-open). The attempt then fails: being no probe,
/// it decides nothing. Its retry finds the route half-open and runs as its
/// probe, and the probe's success closes the circuit.
#[dbtest::test]
async fn a_retry_on_a_half_open_route_runs_as_its_probe(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let job = retried(&w, "fail-once").await;
    let gate = w.openai.gate("fail-once");
    let running = start(&w);
    tokio::time::timeout(REACH, gate.entered.notified())
        .await
        .expect("the first attempt reaches the upstream");
    circuit_until(&w, "clock_timestamp()-interval '1 millisecond'").await;
    gate.release.notify_one();
    tokio::time::timeout(REACH, gate.entered.notified())
        .await
        .expect("the retry reaches the upstream");
    assert_eq!(
        health_row(&w).await,
        (true, 1, true),
        "the retry is the probe"
    );
    gate.release.notify_one();
    assert!(
        tokio::time::timeout(REACH, running)
            .await
            .expect("the batch ends")
            .unwrap()
            .unwrap()
    );
    assert_eq!(w.openai.executed(), ["fail-once", "fail-once"]);
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(1), Some(0))
    );
    assert_eq!(health_row(&w).await, (false, 0, false));
}

/// A line already in flight when its route's circuit opens does not hold a
/// worker while the circuit stays open: its retry, after the usual backoff,
/// finds the route open, sends nothing, and the line fails once its retries
/// are spent. With one worker for the whole runner, another batch's line
/// (another route) then runs.
#[dbtest::test]
async fn a_retry_on_an_open_route_ends_without_sending_and_frees_its_worker(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let mut jobs = w.jobs.clone();
    jobs.limits.batch_workers = 1;
    let runner = Runner::new(jobs);
    let job = retried(&w, "fail").await;
    let gate = w.openai.gate("fail");
    let first = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.run_next().await })
    };
    tokio::time::timeout(REACH, gate.entered.notified())
        .await
        .expect("the line's first attempt reaches the upstream");
    // The route fails for everyone meanwhile: its circuit is open for 300 s.
    circuit_until(&w, "clock_timestamp()+interval '300 seconds'").await;
    let file = w
        .upload(&w.f.principal, &[chat_line("b", "company/claude", "plain")])
        .await;
    let other = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // The other batch starts while the line holds the runner's only worker.
    let second = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.run_next().await })
    };
    gate.release.notify_one();
    for run in [first, second] {
        assert!(
            tokio::time::timeout(REACH, run)
                .await
                .expect("both batches end: the waiting line did not keep the only worker")
                .unwrap()
                .unwrap()
        );
    }
    let (a, b) = (w.reload(&job).await, w.reload(&other).await);
    assert_eq!((a.request_completed, a.request_failed), (Some(0), Some(1)));
    assert_eq!((b.request_completed, b.request_failed), (Some(1), Some(0)));
    assert_eq!(
        w.openai.executed(),
        ["fail"],
        "nothing sent to the open route"
    );
    assert_eq!(w.anthropic.executed(), ["plain"]);
    assert_eq!(health_row(&w).await, (true, 1, false));
}
