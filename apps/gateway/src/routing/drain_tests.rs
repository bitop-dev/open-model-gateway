//! An operator drain (#49 D3) takes a route out of every new attempt and
//! lifts on its own at `until`; a probe on a drained route decides nothing.
use super::*;
use crate::governance::tests::db::fixture;
use crate::inference::repository::InferenceRepository;
use sqlx::PgPool;
use std::time::Duration;

type Fixture = crate::governance::tests::db::Fixture;

/// The deployment of `company/smart`.
async fn route(f: &Fixture) -> Deployment {
    f.store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0)
}

/// Drain `table` row `id` until `until` (SQL), as the management path does.
async fn drain(f: &Fixture, table: &str, id: Uuid, until: &str) {
    sqlx::query(&format!("UPDATE {table} SET drain_reason='maintenance', drain_until={until}, drained_by=(SELECT id FROM users LIMIT 1), drained_at=clock_timestamp() WHERE id=$1"))
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
}

async fn plan_of(f: &Fixture, route: &Deployment) -> Result<RoutePlan, InferenceError> {
    f.store.caches.forget_health(route.id);
    plan_probing(
        &f.store,
        &f.principal,
        "company/smart",
        std::slice::from_ref(route),
        Uuid::new_v4(),
    )
    .await
}

#[dbtest::test]
async fn a_drained_route_takes_no_new_attempt_and_its_drain_lifts_on_its_own(pool: PgPool) {
    let f = fixture(pool).await;
    let route = route(&f).await;
    assert_eq!(
        plan_of(&f, &route).await.unwrap().deployment_ids,
        vec![route.id]
    );
    // Without an end: no `Retry-After`.
    drain(&f, "deployments", route.id, "NULL").await;
    for caching in [true, false] {
        // Through the per-replica caches (`serve` with the config cache on)
        // and without them.
        let mut store = f.store.clone();
        store.caches = std::sync::Arc::new(crate::cache::Caches::new());
        if caching {
            store.caches.versions.enable();
        }
        for result in [
            plan(
                &store,
                &f.principal,
                "company/smart",
                std::slice::from_ref(&route),
                Uuid::new_v4(),
            )
            .await,
            plan_probing(
                &store,
                &f.principal,
                "company/smart",
                std::slice::from_ref(&route),
                Uuid::new_v4(),
            )
            .await,
        ] {
            assert_eq!(
                result,
                Err(InferenceError::RouteDrained(None)),
                "caching {caching}"
            );
        }
    }
    assert_eq!(
        route_state(&f.store, route.id).await.unwrap(),
        RouteState::Drained(None)
    );
    // Ending within the hour: `Retry-After` its remaining time.
    drain(
        &f,
        "deployments",
        route.id,
        "clock_timestamp()+interval '120 seconds'",
    )
    .await;
    match plan_of(&f, &route).await {
        Err(InferenceError::RouteDrained(Some(s))) => assert!((100..=120).contains(&s), "{s}"),
        other => panic!("{other:?}"),
    }
    // Ending later: told only that the model is temporarily unavailable.
    drain(
        &f,
        "deployments",
        route.id,
        "clock_timestamp()+interval '2 hours'",
    )
    .await;
    assert_eq!(
        plan_of(&f, &route).await,
        Err(InferenceError::RouteDrained(None))
    );
    // `until` passed: the drain no longer applies, though it is still stored.
    drain(
        &f,
        "deployments",
        route.id,
        "clock_timestamp()-interval '1 millisecond'",
    )
    .await;
    assert_eq!(
        plan_of(&f, &route).await.unwrap().deployment_ids,
        vec![route.id]
    );
    assert_eq!(
        route_state(&f.store, route.id).await.unwrap(),
        RouteState::Closed
    );
}

#[dbtest::test]
async fn a_drained_connection_drains_every_route_on_it(pool: PgPool) {
    let f = fixture(pool).await;
    let route = route(&f).await;
    let connection: Uuid =
        sqlx::query_scalar("SELECT provider_connection_id FROM deployments WHERE id=$1")
            .bind(route.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    drain(&f, "provider_connections", connection, "NULL").await;
    assert_eq!(
        plan_of(&f, &route).await,
        Err(InferenceError::RouteDrained(None))
    );
    // Both drains apply: the route returns when the later one ends.
    drain(
        &f,
        "provider_connections",
        connection,
        "clock_timestamp()+interval '60 seconds'",
    )
    .await;
    drain(
        &f,
        "deployments",
        route.id,
        "clock_timestamp()+interval '300 seconds'",
    )
    .await;
    match plan_of(&f, &route).await {
        Err(InferenceError::RouteDrained(Some(s))) => assert!((280..=300).contains(&s), "{s}"),
        other => panic!("{other:?}"),
    }
    // Drained and its circuit open too: it returns when the drain ends,
    // not when the cooldown does.
    sqlx::query("INSERT INTO deployment_health(deployment_id,consecutive_failures,open_until,last_observed_at,reopen_count) VALUES($1,3,clock_timestamp()+interval '30 seconds',clock_timestamp(),1)")
        .bind(route.id).execute(&f.store.pool).await.unwrap();
    match plan_of(&f, &route).await {
        Err(InferenceError::RouteDrained(Some(s))) => assert!((280..=300).contains(&s), "{s}"),
        other => panic!("{other:?}"),
    }
}

/// A drained route is never claimed for a probe, a slot claimed before the
/// drain cannot be renewed for a send, and neither the probe's first content
/// nor its result decides the circuit while the route is drained; the slot
/// is freed without a decision. A closed circuit still counts what in-flight
/// requests observe.
#[dbtest::test]
async fn a_probe_on_a_drained_route_decides_nothing(pool: PgPool) {
    let f = fixture(pool).await;
    let route = route(&f).await;
    let hold = Duration::from_secs(60);
    sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,1,60)")
        .bind(route.id).execute(&f.store.pool).await.unwrap();
    record_result(&f.store, route.id, Some(InferenceError::Busy))
        .await
        .unwrap();
    sqlx::query("UPDATE deployment_health SET open_until=clock_timestamp()-interval '1 millisecond' WHERE deployment_id=$1")
        .bind(route.id).execute(&f.store.pool).await.unwrap();
    let ProbeClaim::Claimed(epoch) = claim_probe(&f.store, route.id, Uuid::new_v4(), hold)
        .await
        .unwrap()
    else {
        panic!("half-open before the drain");
    };
    drain(&f, "deployments", route.id, "NULL").await;
    f.store.caches.forget_health(route.id);
    let row = || async {
        sqlx::query_as::<_, (bool, bool, i32)>("SELECT open_until IS NOT NULL, probe_holder IS NOT NULL, reopen_count FROM deployment_health WHERE deployment_id=$1")
            .bind(route.id).fetch_one(&f.store.pool).await.unwrap()
    };
    assert!(!renew_probe(&f.store, route.id, epoch, hold).await.unwrap());
    close_probe(&f.store, route.id, epoch).await.unwrap();
    assert_eq!(row().await, (true, true, 1), "first content closes nothing");
    for result in [None, Some(InferenceError::UpstreamUnavailable)] {
        record_probe_result(&f.store, route.id, result, epoch)
            .await
            .unwrap();
        record_unclaimed_result(&f.store, route.id, result)
            .await
            .unwrap();
        assert_eq!(row().await, (true, true, 1), "{result:?}: no decision");
    }
    release_probe(&f.store, route.id, epoch).await.unwrap();
    assert_eq!(row().await, (true, false, 1));
    assert!(matches!(
        claim_probe(&f.store, route.id, Uuid::new_v4(), hold)
            .await
            .unwrap(),
        ProbeClaim::Busy(_)
    ));
    // Drain cleared: the next request probes and decides again.
    sqlx::query("UPDATE deployments SET drain_reason=NULL,drain_until=NULL,drained_by=NULL,drained_at=NULL WHERE id=$1")
        .bind(route.id).execute(&f.store.pool).await.unwrap();
    let ProbeClaim::Claimed(epoch) = claim_probe(&f.store, route.id, Uuid::new_v4(), hold)
        .await
        .unwrap()
    else {
        panic!("claimed after the drain");
    };
    record_probe_result(&f.store, route.id, None, epoch)
        .await
        .unwrap();
    assert_eq!(row().await, (false, false, 0));
    // Closed and drained: an in-flight request's failure still counts.
    drain(&f, "deployments", route.id, "NULL").await;
    record_result(&f.store, route.id, Some(InferenceError::Busy))
        .await
        .unwrap();
    assert_eq!(row().await, (true, false, 1));
}
