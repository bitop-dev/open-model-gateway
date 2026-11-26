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
