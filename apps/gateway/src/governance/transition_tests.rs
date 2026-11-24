//! Migration 0088: the database refuses a reservation state transition other
//! than pending -> settled/unknown and unknown -> settled, any change to a
//! settled reservation, clearing an unbounded-cost marker, and a hold decrease
//! outside settlement. Where a code path would make the refused write, the
//! test drives it first and gets its usual result (its own guard answers
//! first), then makes the same write directly and gets the database's error.
//! No code path lowers an ordinary request's hold, so those cases are direct
//! writes only; the batch-envelope and realtime exceptions are tested with
//! their own fixtures (`jobs::batch_tests::a_gateway_envelope_may_lower_its_pending_hold`,
//! `a_native_batch_may_not_lower_its_pending_hold` and
//! `governance::realtime_tests::windows_extend_settle_and_finish_on_one_reservation`).
use super::*;
use crate::governance::tests::db::{Fixture, done, fixture, request};
use sqlx::PgPool;

/// The refusal a direct write gets: (constraint, SQLSTATE).
async fn refused(f: &Fixture, sql: &str, id: Uuid) -> (String, String) {
    let error = sqlx::query(sql)
        .bind(id)
        .execute(&f.store.pool)
        .await
        .expect_err(sql);
    let db = error.as_database_error().expect("a database error");
    (
        db.constraint().unwrap_or_default().to_owned(),
        db.code().unwrap_or_default().into_owned(),
    )
}
fn transition() -> (String, String) {
    ("reservation_state_transition".into(), "23514".into())
}
fn hold() -> (String, String) {
    ("reservation_hold_change".into(), "23514".into())
}
fn settled_change() -> (String, String) {
    ("reservation_settled_change".into(), "23514".into())
}
async fn row(f: &Fixture, id: Uuid) -> (String, Option<i64>, Option<i64>) {
    sqlx::query_as(
        "SELECT state,held_microusd,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap()
}
/// A priced reservation admitted for `f`, still pending.
async fn admitted(f: &Fixture) -> Uuid {
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    start.id
}
async fn expire(f: &Fixture, id: Uuid) {
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(reconcile_expired(&f.store, 50).await.unwrap(), 1);
}

#[dbtest::test]
async fn a_settled_reservation_never_changes_state(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let id = admitted(&f).await;
    // Its lease has run out, so only the state keeps reconciliation away.
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    finish(&f.store, &done(id, Some(3), Some(2))).await.unwrap();
    let settled = row(&f, id).await;
    assert_eq!(settled.0, "settled");
    // The code's guard: a different terminal finish of a settled attempt is a
    // conflict, refused before any write; lease reconciliation selects only
    // pending reservations.
    let mut failed = done(id, None, None);
    failed.outcome = Outcome::Failed;
    failed.error = Some(InferenceError::UpstreamUnavailable);
    assert_eq!(
        finish(&f.store, &failed).await,
        Err(InferenceError::Storage)
    );
    assert_eq!(reconcile_expired(&f.store, 50).await.unwrap(), 0);
    // The database: settled -> unknown or pending (actual cleared so no CHECK
    // applies first), and a rewritten amount or usage, which would refund the
    // budget totals derived from it.
    for sql in [
        "UPDATE governance_reservations SET state='unknown',actual_microusd=NULL WHERE execution_id=$1",
        "UPDATE governance_reservations SET state='pending',actual_microusd=NULL WHERE execution_id=$1",
        "UPDATE governance_reservations SET actual_microusd=0,cost_components=NULL WHERE execution_id=$1",
        "UPDATE governance_reservations SET input_tokens=0 WHERE execution_id=$1",
    ] {
        assert_eq!(refused(&f, sql, id).await, settled_change(), "{sql}");
    }
    assert_eq!(row(&f, id).await, settled);
}

#[dbtest::test]
async fn an_unknown_reservation_never_returns_to_pending(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let id = admitted(&f).await;
    expire(&f, id).await;
    let unknown = row(&f, id).await;
    assert_eq!(unknown.0, "unknown");
    // The code's guard: nothing reopens it. Reconciling again finds nothing,
    // and a late finish of the attempt is refused (its execution is no
    // longer running).
    assert_eq!(reconcile_expired(&f.store, 50).await.unwrap(), 0);
    assert_eq!(
        finish(&f.store, &done(id, Some(3), Some(2))).await,
        Err(InferenceError::Storage)
    );
    assert_eq!(
        refused(
            &f,
            "UPDATE governance_reservations SET state='pending' WHERE execution_id=$1",
            id
        )
        .await,
        transition()
    );
    assert_eq!(row(&f, id).await, unknown);
}

#[dbtest::test]
async fn a_settled_reservation_never_changes_its_hold(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let id = admitted(&f).await;
    expire(&f, id).await;
    let evidence = Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
        ..Default::default()
    };
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        id,
        evidence,
        "receipt:1",
        f.owner,
    )
    .await
    .unwrap();
    let settled = row(&f, id).await;
    assert_eq!(settled.0, "settled");
    // The code's guard: other evidence for a settled reservation is refused.
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            id,
            evidence,
            "receipt:2",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    for sql in [
        "UPDATE governance_reservations SET held_microusd=held_microusd+1 WHERE execution_id=$1",
        "UPDATE governance_reservations SET held_microusd=0 WHERE execution_id=$1",
        "UPDATE governance_reservations SET held_microusd=NULL WHERE execution_id=$1",
    ] {
        assert_eq!(refused(&f, sql, id).await, settled_change(), "{sql}");
    }
    assert_eq!(row(&f, id).await, settled);
}

#[dbtest::test]
async fn a_hold_never_decreases_outside_settlement(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let pending = admitted(&f).await;
    let unknown = admitted(&f).await;
    expire(&f, unknown).await;
    let decreases = [
        "UPDATE governance_reservations SET held_microusd=held_microusd-1 WHERE execution_id=$1",
        "UPDATE governance_reservations SET held_microusd=0 WHERE execution_id=$1",
    ];
    for id in [pending, unknown] {
        let before = row(&f, id).await;
        assert!(before.1.is_some_and(|h| h > 0));
        for sql in decreases {
            assert_eq!(refused(&f, sql, id).await, hold(), "{} {sql}", before.0);
        }
        assert_eq!(row(&f, id).await, before);
        // Growing is not a decrease, nor is becoming unbounded (NULL).
        for sql in [
            "UPDATE governance_reservations SET held_microusd=held_microusd+1 WHERE execution_id=$1",
            "UPDATE governance_reservations SET held_microusd=NULL WHERE execution_id=$1",
        ] {
            sqlx::query(sql)
                .bind(id)
                .execute(&f.store.pool)
                .await
                .unwrap();
        }
        // From unbounded back to a value is one.
        assert_eq!(
            refused(
                &f,
                "UPDATE governance_reservations SET held_microusd=5 WHERE execution_id=$1",
                id
            )
            .await,
            hold()
        );
    }
    // Ending a pending attempt as unknown keeps its hold: unknown cost
    // retains holds. Only the known floor of an unbounded hold may be
    // recorded, and only with the unbounded marker kept (settlement's
    // greatest(NULL, floor)).
    let ending = admitted(&f).await;
    for sql in [
        "UPDATE governance_reservations SET state='unknown',held_microusd=held_microusd-1 WHERE execution_id=$1",
        "UPDATE governance_reservations SET state='unknown',held_microusd=0 WHERE execution_id=$1",
    ] {
        assert_eq!(refused(&f, sql, ending).await, hold(), "{sql}");
    }
    sqlx::query("UPDATE governance_reservations SET held_microusd=NULL,unbounded_cost=true WHERE execution_id=$1")
        .bind(ending)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        refused(
            &f,
            "UPDATE governance_reservations SET state='unknown',held_microusd=5,unbounded_cost=false WHERE execution_id=$1",
            ending
        )
        .await,
        hold()
    );
    sqlx::query("UPDATE governance_reservations SET state='unknown',held_microusd=5 WHERE execution_id=$1 AND state='pending'")
        .bind(ending)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(row(&f, ending).await, ("unknown".into(), Some(5), None));
    // Settling counts the amount, not the hold: any hold may go with it.
    let settling = admitted(&f).await;
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=0,input_tokens=0,output_tokens=0,held_microusd=0 WHERE execution_id=$1")
        .bind(settling)
        .execute(&f.store.pool)
        .await
        .unwrap();
}

#[dbtest::test]
async fn an_unbounded_marker_is_never_cleared(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let pending = admitted(&f).await;
    let unknown = admitted(&f).await;
    expire(&f, unknown).await;
    for id in [pending, unknown] {
        // Setting it is the conservative direction (every writer only ORs it).
        sqlx::query("UPDATE governance_reservations SET unbounded_cost=true WHERE execution_id=$1")
            .bind(id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            refused(
                &f,
                "UPDATE governance_reservations SET unbounded_cost=false WHERE execution_id=$1",
                id
            )
            .await,
            hold()
        );
    }
}

/// The trigger is cloned onto every attached partition, the legacy one and
/// the months `omg_ensure_partitions` creates later, and refuses there too.
#[dbtest::test]
async fn every_reservation_partition_has_the_guard(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::query("SELECT omg_ensure_partitions(12)")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid
         WHERE i.inhparent='public.governance_reservations'::regclass
           AND NOT EXISTS(SELECT FROM pg_trigger t WHERE t.tgrelid=c.oid
             AND t.tgname='governance_reservations_transition' AND t.tgenabled<>'D')
         ORDER BY 1",
    )
    .fetch_all(&f.store.pool)
    .await
    .unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    let partitions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_inherits WHERE inhparent='public.governance_reservations'::regclass",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert!(partitions >= 13, "legacy, current and twelve future months");
    // A settled row in the newest month refuses a transition like any other.
    f.price(1_000_000).await;
    let id = admitted(&f).await;
    finish(&f.store, &done(id, Some(3), Some(2))).await.unwrap();
    let late: UtcInstant =
        sqlx::query_scalar("SELECT date_trunc('month',now(),'UTC')+interval '11 months'")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    let moved = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,input_tokens,output_tokens) SELECT $1,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,'succeeded',$1,$2,3,2 FROM inference_executions WHERE id=$3")
        .bind(moved).bind(late).bind(id).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,actual_microusd,input_tokens,output_tokens) SELECT $1,workspace_id,api_key_id,deployment_id,price_id,$2,date_trunc('minute',$2::timestamptz,'UTC'),date_trunc('month',$2::timestamptz,'UTC'),$2,'settled',reserved_tokens,held_microusd,5,3,2 FROM governance_reservations WHERE execution_id=$3")
        .bind(moved).bind(late).bind(id).execute(&f.store.pool).await.unwrap();
    assert_eq!(
        refused(
            &f,
            "UPDATE governance_reservations SET state='unknown',actual_microusd=NULL WHERE execution_id=$1",
            moved
        )
        .await,
        settled_change()
    );
}
