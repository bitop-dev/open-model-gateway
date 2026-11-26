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

/// A gateway-run batch of one `fail-once` line with one retry, on a route
/// whose circuit opens on one failure for `cooldown` seconds.
async fn retried(pool: PgPool, cooldown: i32) -> (World, JobRow) {
    let w = world(pool, false, false, 1).await;
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,$2) ON CONFLICT (deployment_id) DO UPDATE SET failure_threshold=1,cooldown_seconds=$2")
        .bind(w.f.deployment).bind(cooldown).execute(&w.f.store.pool).await.unwrap();
    let file = w
        .upload(
            &w.f.principal,
            &[chat_line("a", "company/smart", "fail-once")],
        )
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
    (w, job)
}

/// The line's first attempt fails and opens the route's circuit (5 s). Its
/// retry is not sent while the circuit is open (the retry's own backoff is
/// 2 s): it waits for the cooldown, then runs as the half-open route's probe,
/// and its success closes the circuit.
#[dbtest::test]
async fn a_retry_waits_out_an_open_circuit_then_probes(pool: PgPool) {
    let (w, job) = retried(pool, 5).await;
    let started = std::time::Instant::now();
    assert!(
        tokio::time::timeout(REACH, start(&w))
            .await
            .expect("the retry runs once the circuit is half-open")
            .unwrap()
            .unwrap()
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(w.openai.executed(), ["fail-once", "fail-once"]);
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(1), Some(0))
    );
    let (open, reopens, held): (bool, i32, bool) = sqlx::query_as(
        "SELECT open_until IS NOT NULL, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(w.f.deployment)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!((open, reopens, held), (false, 0, false));
}

/// A retry waiting on an open circuit (300 s) still stops with its batch:
/// cancelled, the run ends at once and the retry is never sent.
#[dbtest::test]
async fn a_retry_waiting_on_an_open_circuit_stops_when_the_batch_is_cancelled(pool: PgPool) {
    let (w, job) = retried(pool, 300).await;
    let running = start(&w);
    // The first attempt failed and the circuit is open.
    tokio::time::timeout(REACH, async {
        loop {
            let open: Option<bool> = sqlx::query_scalar(
                "SELECT open_until > now() FROM deployment_health WHERE deployment_id=$1",
            )
            .bind(w.f.deployment)
            .fetch_optional(&w.f.store.pool)
            .await
            .unwrap()
            .flatten();
            if open == Some(true) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    w.jobs
        .cancel_batch(&w.f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    let ended = std::time::Instant::now();
    assert!(
        tokio::time::timeout(REACH, running)
            .await
            .expect("the run stops while its retry waits")
            .unwrap()
            .unwrap()
    );
    assert!(
        ended.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        ended.elapsed()
    );
    assert_eq!(
        w.openai.executed(),
        ["fail-once"],
        "the retry was never sent"
    );
    assert_eq!(w.reload(&job).await.state, "cancelled");
}
