//! `bootstrap-dev` against a concurrent first OIDC sign-in, on the real
//! functions. Both take `lifecycle::lock` (the catalog advisory lock and the
//! installation row) before reading or writing users: sign-in its shared form
//! first, the seed its exclusive form before its emptiness check. At READ
//! COMMITTED each statement sees what committed before it, so the seed either
//! sees the new account and refuses, or commits before the sign-in starts.
use super::*;
use std::time::Duration;

const EMAIL: &str = "first@example.test";

/// Waits until some backend is blocked on a lock, so the next step runs
/// while that transaction is known to be in flight.
async fn until_waiting(pool: &sqlx::PgPool, waiting: i64) {
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE NOT granted")
            .fetch_one(pool)
            .await
            .unwrap();
        if n >= waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no backend reached the lock");
}

fn sign_in(store: &Store) -> tokio::task::JoinHandle<Result<uuid::Uuid, AuthError>> {
    let store = store.clone();
    tokio::spawn(async move {
        resolve_identity(&store, "https://issuer.test", "first-subject", EMAIL, &[]).await
    })
}

fn bootstrap(
    store: &Store,
) -> tokio::task::JoinHandle<anyhow::Result<Option<crate::bootstrap::DevelopmentKeys>>> {
    let store = store.clone();
    tokio::spawn(async move {
        crate::bootstrap::seed(&store, crate::config::Environment::Development).await
    })
}

#[dbtest::test]
async fn a_sign_in_in_flight_makes_the_bootstrap_refuse(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    // Holds the sign-in after its installation locks, just before it creates
    // the account (the per-email lock is the last one it takes).
    let mut gate = crate::db::begin(&pool).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("email:{EMAIL}"))
        .execute(&mut *gate)
        .await
        .unwrap();
    let signing_in = sign_in(&store);
    until_waiting(&pool, 1).await;
    let seeding = bootstrap(&store);
    until_waiting(&pool, 2).await;
    assert!(!seeding.is_finished(), "the seed waits for the sign-in");
    gate.commit().await.unwrap();
    // No platform access is granted by authentication alone.
    assert!(signing_in.await.unwrap().is_err());
    let refused = seeding.await.unwrap().err().unwrap();
    assert!(
        refused.to_string().contains("requires an empty"),
        "{refused}"
    );
    let state: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM users),(SELECT count(*) FROM platform_role_grants),(SELECT count(*) FROM api_keys)").fetch_one(&pool).await.unwrap();
    assert_eq!(state, (1, 0, 0));
}

#[dbtest::test]
async fn a_sign_in_during_the_bootstrap_waits_for_it(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    // Holds the seed after its exclusive catalog lock, at the installation row.
    let mut gate = crate::db::begin(&pool).await.unwrap();
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *gate)
        .await
        .unwrap();
    let seeding = bootstrap(&store);
    until_waiting(&pool, 1).await;
    let signing_in = sign_in(&store);
    until_waiting(&pool, 2).await;
    assert!(!signing_in.is_finished(), "the sign-in waits for the seed");
    gate.commit().await.unwrap();
    assert!(seeding.await.unwrap().unwrap().is_some());
    assert!(signing_in.await.unwrap().is_err());
    let users: Vec<String> = sqlx::query_scalar("SELECT email FROM users ORDER BY created_at")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(users, ["developer@local.invalid", EMAIL]);
}
