//! A batch line's attempt on its pinned route is planned as the route is when
//! the engine sends it (#49): an attempt holding route capacity is sent only
//! while the circuit is closed, or as the half-open route's probe; an
//! ungated attempt, which admission always refuses, is planned as before.
use super::*;
use crate::governance::tests::db::fixture;
use crate::inference::repository::InferenceRepository;
use crate::jobs::fence::BatchFence;
use sqlx::PgPool;

/// A line repository of a batch that exists only for planning.
fn lines(f: &crate::governance::tests::db::Fixture) -> LineRepository {
    LineRepository {
        store: f.store.clone(),
        batch: crate::governance::batch::LineContext {
            envelope: Uuid::new_v4(),
            fence: BatchFence {
                job: Uuid::new_v4(),
                runner: Uuid::new_v4(),
                epoch: 1,
            },
        },
        pins: Mutex::default(),
        claimed: Mutex::default(),
        probes: Mutex::default(),
    }
}

#[dbtest::test]
async fn a_pinned_attempt_is_planned_as_its_route_is_now(pool: PgPool) {
    use crate::routing::RoutePlan;
    let f = fixture(pool).await;
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let route = deployment.id;
    let repository = lines(&f);
    let only = std::slice::from_ref(&deployment);
    let (gated, ungated, held) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    repository.pin(gated, route, None, true, None);
    repository.pin(ungated, route, None, false, None);
    repository.pin(held, route, None, true, Some((route, 7)));
    // A slot claimed on another route is never handed over for this one.
    let elsewhere = Uuid::new_v4();
    repository.pin(elsewhere, route, None, true, Some((Uuid::new_v4(), 7)));
    let planned =
        |execution| repository.route_plan_probing(&f.principal, "company/smart", only, execution);
    let circuit = |until: &'static str| {
        let store = f.store.clone();
        async move {
            sqlx::query(&format!("INSERT INTO deployment_health(deployment_id,consecutive_failures,open_until,last_observed_at,reopen_count) VALUES($1,3,{until},clock_timestamp(),1) ON CONFLICT (deployment_id) DO UPDATE SET open_until=EXCLUDED.open_until"))
                .bind(route)
                .execute(&store.pool)
                .await
                .unwrap();
            store.caches.forget_health(route);
        }
    };
    let single = RoutePlan::single(vec![route]);
    // Closed: sent as planned.
    assert_eq!(planned(gated).await.unwrap(), single);
    // Open: a gated attempt sends nothing.
    circuit("clock_timestamp()+interval '60 seconds'").await;
    assert!(matches!(
        planned(gated).await,
        Err(InferenceError::RouteCoolingDown(s)) if (1..=60).contains(&s)
    ));
    // Half-open: only as its probe (the engine claims the slot).
    circuit("clock_timestamp()-interval '1 millisecond'").await;
    assert_eq!(
        planned(gated).await.unwrap(),
        RoutePlan {
            probes: vec![route],
            ..single.clone()
        }
    );
    // The slot the line claimed at dispatch: handed to the engine to renew.
    assert_eq!(
        planned(held).await.unwrap(),
        RoutePlan {
            held_probe: Some((route, 7)),
            ..single.clone()
        }
    );
    assert_eq!(
        planned(elsewhere).await.unwrap(),
        RoutePlan {
            probes: vec![route],
            ..single.clone()
        }
    );
    // Ungated: planned as before in every state, and admission refuses it.
    for until in [
        "clock_timestamp()+interval '60 seconds'",
        "clock_timestamp()-interval '1 millisecond'",
    ] {
        circuit(until).await;
        assert_eq!(planned(ungated).await.unwrap(), single);
    }
}

/// An ungated attempt with no pin and no creation-time plan (no route could
/// take the line) is planned as a caller that decides nothing: a half-open
/// route is no probe for it, so the engine makes no claim, and the slot
/// stays free for a request that can be admitted.
#[dbtest::test]
async fn an_unpinned_ungated_attempt_claims_no_probe_slot(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let repository = lines(&f);
    sqlx::query("INSERT INTO deployment_health(deployment_id,consecutive_failures,open_until,last_observed_at,reopen_count) VALUES($1,3,clock_timestamp()-interval '1 millisecond',clock_timestamp(),1)")
        .bind(deployment.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let only = std::slice::from_ref(&deployment);
    // A caller whose attempts decide would be the probe here.
    let probing = f
        .store
        .route_plan_probing(&f.principal, "company/smart", only, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(probing.probes, vec![deployment.id]);
    let planned = repository
        .route_plan_probing(&f.principal, "company/smart", only, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        (planned.deployment_ids, planned.probes, planned.held_probe),
        (vec![deployment.id], vec![], None)
    );
    let held: bool = sqlx::query_scalar(
        "SELECT probe_holder IS NOT NULL FROM deployment_health WHERE deployment_id=$1",
    )
    .bind(deployment.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert!(!held);
}
