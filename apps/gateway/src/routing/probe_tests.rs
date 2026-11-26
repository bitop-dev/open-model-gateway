//! Half-open routes (#49): after a cooldown, one real request (the probe)
//! decides whether traffic resumes; one prober per route across replicas;
//! openings back off; a probe slot cannot stick.
use super::*;
use crate::governance::tests::db::fixture;
use crate::inference::repository::InferenceRepository;
use sqlx::PgPool;
use std::time::Duration;

/// The deployment of `company/smart`, with its circuit open (threshold 1,
/// `cooldown` seconds): one counted failure.
async fn opened(f: &crate::governance::tests::db::Fixture, cooldown: i32) -> Deployment {
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,$2)")
        .bind(deployment.id).bind(cooldown).execute(&f.store.pool).await.unwrap();
    record_result(&f.store, deployment.id, Some(InferenceError::Busy))
        .await
        .unwrap();
    deployment
}

/// The circuit's cooldown passes now.
async fn expire(f: &crate::governance::tests::db::Fixture, deployment: Uuid) {
    sqlx::query("UPDATE deployment_health SET open_until=clock_timestamp()-interval '1 millisecond' WHERE deployment_id=$1")
        .bind(deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    f.store.caches.forget_health(deployment);
}

/// (seconds the circuit stays open from now, rounded; reopen count; probe held).
async fn circuit(
    f: &crate::governance::tests::db::Fixture,
    deployment: Uuid,
) -> (Option<i64>, i32, bool) {
    sqlx::query_as("SELECT round(EXTRACT(EPOCH FROM open_until-clock_timestamp()))::int8, reopen_count, probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1")
        .bind(deployment)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

const HOLD: Duration = Duration::from_secs(60);

/// Claim the half-open route's probe slot; its epoch.
async fn claimed(f: &crate::governance::tests::db::Fixture, deployment: Uuid) -> i64 {
    match claim_probe(&f.store, deployment, Uuid::new_v4(), HOLD)
        .await
        .unwrap()
    {
        ProbeClaim::Claimed(epoch) => epoch,
        other => panic!("not claimed: {other:?}"),
    }
}

#[dbtest::test]
async fn an_expired_cooldown_admits_one_probe_across_replicas(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    let only = std::slice::from_ref(&deployment);
    let probing = || {
        plan_probing(
            &f.store,
            &f.principal,
            "company/smart",
            only,
            Uuid::new_v4(),
        )
    };
    assert!(matches!(
        probing().await,
        Err(InferenceError::RouteCoolingDown(_))
    ));
    expire(&f, deployment.id).await;
    // Half-open: a probing plan lists the route as a probe.
    let planned = probing().await.unwrap();
    assert_eq!(
        (planned.deployment_ids.clone(), planned.probes.clone()),
        (vec![deployment.id], vec![deployment.id])
    );
    // Two replicas (each with its own caches) claim at once: one wins.
    let mut other = f.store.clone();
    other.caches = std::sync::Arc::new(crate::cache::Caches::new());
    let (a, b) = tokio::join!(
        claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD),
        claim_probe(&other, deployment.id, Uuid::new_v4(), HOLD),
    );
    let claims = [a.unwrap(), b.unwrap()];
    assert_eq!(
        claims
            .iter()
            .filter(|c| matches!(c, ProbeClaim::Claimed(_)))
            .count(),
        1,
        "{claims:?}"
    );
    assert_eq!(
        claims
            .iter()
            .filter(|c| matches!(c, ProbeClaim::Busy(_)))
            .count(),
        1,
        "{claims:?}"
    );
    // While it is held, a probing plan sees the route cooling down for the
    // slot's remaining time, on both replicas.
    for store in [&f.store, &other] {
        match plan_probing(store, &f.principal, "company/smart", only, Uuid::new_v4()).await {
            Err(InferenceError::RouteCoolingDown(seconds)) => assert!((1..=60).contains(&seconds)),
            other => panic!("expected cooling down, got {other:?}"),
        }
    }
    // A caller that records no health still reaches it, as an expired cooldown always did.
    let unrecorded = plan(
        &f.store,
        &f.principal,
        "company/smart",
        only,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    assert_eq!(
        (unrecorded.deployment_ids, unrecorded.probes),
        (vec![deployment.id], vec![])
    );
}

#[dbtest::test]
async fn a_failed_probe_reopens_for_a_doubled_cooldown_up_to_the_cap(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    assert_eq!(circuit(&f, deployment.id).await, (Some(60), 1, false));
    for (open, reopens) in [(120, 2), (240, 3), (300, 4), (300, 5)] {
        expire(&f, deployment.id).await;
        let epoch = claimed(&f, deployment.id).await;
        assert!(circuit(&f, deployment.id).await.2);
        record_probe_result(
            &f.store,
            deployment.id,
            Some(InferenceError::UpstreamUnavailable),
            epoch,
        )
        .await
        .unwrap();
        // The probe's failure reopened the circuit and ended the slot.
        assert_eq!(
            circuit(&f, deployment.id).await,
            (Some(open), reopens, false)
        );
    }
    // A failure while the circuit is open changes nothing more.
    record_result(&f.store, deployment.id, Some(InferenceError::Timeout))
        .await
        .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(300), 5, false));
    // A cooldown longer than 300 s is its own cap.
    sqlx::query("UPDATE deployment_routing SET cooldown_seconds=600 WHERE deployment_id=$1")
        .bind(deployment.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    expire(&f, deployment.id).await;
    let epoch = claimed(&f, deployment.id).await;
    record_probe_result(&f.store, deployment.id, Some(InferenceError::Busy), epoch)
        .await
        .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(600), 6, false));
    assert!(health(&f.store, deployment.id).await.unwrap().circuit_open);
}

/// Only the probe decides an open or half-open circuit: a result without
/// its current epoch (an attempt admitted before the circuit opened, a
/// realtime session, a probe whose slot ended) changes nothing, success or
/// failure, and leaves the probe's slot alone.
#[dbtest::test]
async fn only_the_current_probe_decides_an_open_circuit(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    // Open: a late success does not close it.
    record_result(&f.store, deployment.id, None).await.unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(60), 1, false));
    // Half-open with a probe running: neither a late success nor a late
    // failure decides, and the slot stays the probe's.
    expire(&f, deployment.id).await;
    let stale = claimed(&f, deployment.id).await;
    release_probe(&f.store, deployment.id, stale).await.unwrap();
    let epoch = claimed(&f, deployment.id).await;
    for result in [None, Some(InferenceError::UpstreamUnavailable)] {
        record_result(&f.store, deployment.id, result)
            .await
            .unwrap();
        record_probe_result(&f.store, deployment.id, result, stale)
            .await
            .unwrap();
        let (open, reopens, held) = circuit(&f, deployment.id).await;
        assert!(
            open.is_some_and(|s| s <= 0) && reopens == 1 && held,
            "{open:?} {reopens} {held}"
        );
    }
    // The probe's own success decides.
    record_probe_result(&f.store, deployment.id, None, epoch)
        .await
        .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 0, false));
    // Closed again, ordinary results count as before.
    record_result(&f.store, deployment.id, Some(InferenceError::Busy))
        .await
        .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(60), 1, false));
}

/// The route's state as a request about to send sees it.
#[dbtest::test]
async fn route_state_reads_the_circuit_now(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        route_state(&f.store, deployment.id).await.unwrap(),
        RouteState::Closed
    );
    let deployment = opened(&f, 60).await;
    assert!(
        matches!(route_state(&f.store, deployment.id).await.unwrap(), RouteState::Open(s) if (1..=60).contains(&s))
    );
    expire(&f, deployment.id).await;
    assert_eq!(
        route_state(&f.store, deployment.id).await.unwrap(),
        RouteState::HalfOpen
    );
}
#[dbtest::test]
async fn a_successful_probe_closes_and_forgets_the_openings(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    expire(&f, deployment.id).await;
    let epoch = claimed(&f, deployment.id).await;
    record_probe_result(&f.store, deployment.id, None, epoch)
        .await
        .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 0, false));
    let only = std::slice::from_ref(&deployment);
    let planned = plan_probing(
        &f.store,
        &f.principal,
        "company/smart",
        only,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    assert_eq!(
        (planned.deployment_ids, planned.probes),
        (vec![deployment.id], vec![])
    );
    // Closed: no claim is made (there is nothing to claim).
    assert_eq!(
        claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
            .await
            .unwrap(),
        ProbeClaim::Closed
    );
}
#[dbtest::test]
async fn a_streams_first_content_closes_but_only_a_complete_success_forgets(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    sqlx::query("UPDATE deployment_routing SET failure_threshold=2 WHERE deployment_id=$1")
        .bind(deployment.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    // First content, then a failure: the circuit closed at the content, and
    // the failure is an ordinary one (below the threshold of 2).
    expire(&f, deployment.id).await;
    let ProbeClaim::Claimed(epoch) = claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
        .await
        .unwrap()
    else {
        panic!()
    };
    close_probe(&f.store, deployment.id, epoch).await.unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 1, false));
    record_result(
        &f.store,
        deployment.id,
        Some(InferenceError::UpstreamUnavailable),
    )
    .await
    .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 1, false));
    // The next counted failure reaches the threshold: the reopening continues the backoff.
    record_result(
        &f.store,
        deployment.id,
        Some(InferenceError::UpstreamUnavailable),
    )
    .await
    .unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(120), 2, false));
    // First content, then `Done` (a complete success): the openings are forgotten.
    expire(&f, deployment.id).await;
    let ProbeClaim::Claimed(epoch) = claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
        .await
        .unwrap()
    else {
        panic!()
    };
    close_probe(&f.store, deployment.id, epoch).await.unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 2, false));
    record_result(&f.store, deployment.id, None).await.unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (None, 0, false));
}

#[dbtest::test]
async fn a_probe_slot_ends_without_a_decision(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    expire(&f, deployment.id).await;
    // Released without a decision (cancelled, no health signal, refused at
    // admission): the next request may probe at once.
    let ProbeClaim::Claimed(first) = claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
        .await
        .unwrap()
    else {
        panic!()
    };
    release_probe(&f.store, deployment.id, first).await.unwrap();
    assert_eq!(circuit(&f, deployment.id).await, (Some(0), 1, false));
    // The holder dies (nothing is written): the slot ends at `probe_until`.
    let ProbeClaim::Claimed(dead) = claim_probe(
        &f.store,
        deployment.id,
        Uuid::new_v4(),
        Duration::from_millis(200),
    )
    .await
    .unwrap() else {
        panic!()
    };
    assert!(matches!(
        claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
            .await
            .unwrap(),
        ProbeClaim::Busy(_)
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ProbeClaim::Claimed(next) = claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD)
        .await
        .unwrap()
    else {
        panic!("the expired slot was not freed")
    };
    assert!(next > dead && dead > first);
    // The dead holder's late writes are fenced: they neither free nor close the new slot.
    release_probe(&f.store, deployment.id, dead).await.unwrap();
    close_probe(&f.store, deployment.id, dead).await.unwrap();
    assert!(circuit(&f, deployment.id).await.2);
    assert!(
        health(&f.store, deployment.id)
            .await
            .unwrap()
            .open_until
            .is_some()
    );
    // A claim on a route whose circuit reopened meanwhile (the probe failed)
    // is busy, not claimed.
    record_probe_result(&f.store, deployment.id, Some(InferenceError::Busy), next)
        .await
        .unwrap();
    assert!(
        matches!(claim_probe(&f.store, deployment.id, Uuid::new_v4(), HOLD).await.unwrap(), ProbeClaim::Busy(seconds) if seconds > 60)
    );
}

/// A route whose circuit never opened costs what it did: the probing plan
/// and a success make the same statements as the plan for a caller that
/// records no health, and nothing touches the probe slot. (The engine never
/// claims for a plan without probes: `inference::tests`.)
#[dbtest::test]
#[ignore = "local statement-count fixture; PostgreSQL must preload pg_stat_statements"]
async fn a_healthy_route_makes_no_probe_statement(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    sqlx::raw_sql("CREATE SCHEMA probe_metrics; CREATE EXTENSION pg_stat_statements WITH SCHEMA probe_metrics")
        .execute(&pool).await.unwrap();
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let only = std::slice::from_ref(&deployment);
    record_result(&f.store, deployment.id, None).await.unwrap();
    let calls = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT coalesce(sum(calls),0)::int8 FROM probe_metrics.pg_stat_statements
             WHERE dbid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND query NOT LIKE '%pg_stat_statements%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let probe_calls = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT coalesce(sum(calls),0)::int8 FROM probe_metrics.pg_stat_statements
             WHERE dbid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND (query LIKE '%probe_epoch=probe_epoch%' OR query LIKE '%probe_holder=NULL, probe_until=NULL%')",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let mut cost = Vec::new();
    for probing in [false, true] {
        f.store.caches.forget_health(deployment.id);
        let before = calls().await;
        for _ in 0..5 {
            let planned = if probing {
                plan_probing(
                    &f.store,
                    &f.principal,
                    "company/smart",
                    only,
                    Uuid::new_v4(),
                )
                .await
            } else {
                plan(
                    &f.store,
                    &f.principal,
                    "company/smart",
                    only,
                    Uuid::new_v4(),
                )
                .await
            }
            .unwrap();
            assert!(planned.probes.is_empty());
            record_result(&f.store, deployment.id, None).await.unwrap();
        }
        cost.push(calls().await - before);
    }
    eprintln!("statements of five healthy requests (plan, probing plan): {cost:?}");
    assert_eq!(
        cost[0], cost[1],
        "statements of five healthy requests: {cost:?}"
    );
    assert_eq!(probe_calls().await, 0);
}

/// A batch line claims its route's slot before it waits for capacity; at
/// send time the engine renews the slot for its own deadline. Only the
/// holder of an unexpired slot renews it: a claim that expired meanwhile,
/// or was superseded by a newer probe, is lost, and the line sends nothing.
#[dbtest::test]
async fn only_an_unexpired_held_slot_is_renewed_for_the_send(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = opened(&f, 60).await;
    expire(&f, deployment.id).await;
    let held = match claim_probe(
        &f.store,
        deployment.id,
        Uuid::new_v4(),
        Duration::from_secs(1),
    )
    .await
    .unwrap()
    {
        ProbeClaim::Claimed(epoch) => epoch,
        other => panic!("not claimed: {other:?}"),
    };
    assert!(
        renew_probe(&f.store, deployment.id, held, HOLD)
            .await
            .unwrap()
    );
    let until: i64 = sqlx::query_scalar(
        "SELECT round(EXTRACT(EPOCH FROM probe_until-clock_timestamp()))::int8 FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(deployment.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(until, 60, "rebased on the send's own hold");
    // Expired before the send: lost, and the slot is not revived.
    sqlx::query("UPDATE deployment_health SET probe_until=clock_timestamp()-interval '1 millisecond' WHERE deployment_id=$1")
        .bind(deployment.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        !renew_probe(&f.store, deployment.id, held, HOLD)
            .await
            .unwrap()
    );
    // Superseded: a newer probe holds the slot; the old epoch cannot extend it.
    let newer = claimed(&f, deployment.id).await;
    assert!(newer > held);
    assert!(
        !renew_probe(&f.store, deployment.id, held, HOLD)
            .await
            .unwrap()
    );
    // Released: nothing to renew.
    release_probe(&f.store, deployment.id, newer).await.unwrap();
    assert!(
        !renew_probe(&f.store, deployment.id, newer, HOLD)
            .await
            .unwrap()
    );
    assert_eq!(circuit(&f, deployment.id).await.2, false);
}
