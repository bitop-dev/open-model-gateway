#![cfg(feature = "integration-tests")]
use open_model_gateway::store::{MIGRATOR, Store};
use sqlx::PgPool;
use uuid::Uuid;
#[sqlx::test(migrations = false)]
async fn scheduled_reports_0063_upgrade_preserves_ordered_prefix_and_refuses_partial_readiness(
    pool: PgPool,
) {
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 63)
        .expect("0063 reserved for report generation");
    assert_eq!(migration.description, "report generation");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 63)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let prefix: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let store = Store::new(pool.clone());
    assert!(!store.readiness().await.schema);
    store.migrate_enterprise().await.unwrap();
    store.preflight_enterprise().await.unwrap();
    assert!(store.readiness().await.schema);
    assert_eq!(
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<63 ORDER BY version"
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        prefix
    );
    for table in [
        "report_runs",
        "report_snapshots",
        "report_generation_claims",
        "report_generation_outcomes",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "no backfill: {table}"
        );
    }
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=63")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.readiness().await.schema);
}

#[sqlx::test(migrations = false)]
async fn scheduled_reports_0084_upgrade_fences_old_claims_and_keeps_pending_cursor(pool: PgPool) {
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 84)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let old = Uuid::new_v4();
    let epoch: i64 = sqlx::query_scalar("SELECT omg_lease_acquire('report_scheduler',$1,3600)")
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("WITH creator AS (INSERT INTO users(id,email) VALUES(gen_random_uuid(),'report-upgrade@example.test') RETURNING id) INSERT INTO report_schedules(scope_kind,created_by,cadence,timezone,delivery_time,state,next_period_end,next_run_at) SELECT 'installation',id,'weekly','America/Havana','09:00','active','2026-11-23 00:00Z','2026-11-23 14:00Z' FROM creator RETURNING id")
        .fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO report_generation_claims(schedule_id,token,configuration_revision,period_start,period_end,skipped_intervals,expires_at) VALUES($1,gen_random_uuid(),1,'2026-11-02 00:00Z','2026-11-09 00:00Z',0,clock_timestamp()+interval '30 seconds')")
        .bind(id).execute(&pool).await.unwrap();
    let published: Uuid = sqlx::query_scalar("INSERT INTO report_runs(schedule_id,period_start,period_end,configuration_revision,claim_token,lease_holder,lease_epoch,skipped_intervals,observed_at) SELECT schedule_id,period_start,period_end,configuration_revision,token,$2,$3,skipped_intervals,clock_timestamp() FROM report_generation_claims WHERE schedule_id=$1 RETURNING id")
        .bind(id).bind(old).bind(epoch).fetch_one(&pool).await.unwrap();
    let snapshot = serde_json::json!({"fixture":"already published"});
    sqlx::query("INSERT INTO report_snapshots(run_id,body) VALUES($1,$2)")
        .bind(published)
        .bind(&snapshot)
        .execute(&pool)
        .await
        .unwrap();
    let token: Uuid = sqlx::query_scalar("UPDATE report_generation_claims SET token=gen_random_uuid(),period_start='2026-11-16 00:00Z',period_end='2026-11-23 00:00Z',expires_at=clock_timestamp()+interval '30 seconds' WHERE schedule_id=$1 RETURNING token")
        .bind(id).fetch_one(&pool).await.unwrap();

    let store = Store::new(pool.clone());
    assert!(!store.is_ready().await);
    store.migrate_enterprise().await.unwrap();
    assert!(store.is_ready().await);
    assert!(sqlx::query_scalar::<_, bool>("SELECT configuration_revision=1 AND next_period_end='2026-11-23 00:00Z' AND next_run_at='2026-11-23 14:00Z' AND cursor_tzdb='' FROM report_schedules WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap());
    let refused = sqlx::query("INSERT INTO report_runs(schedule_id,period_start,period_end,configuration_revision,claim_token,lease_holder,lease_epoch,skipped_intervals,observed_at) SELECT schedule_id,period_start,period_end,configuration_revision,token,$2,$3,skipped_intervals,clock_timestamp() FROM report_generation_claims WHERE token=$1")
        .bind(token).bind(old).bind(epoch).execute(&pool).await.unwrap_err();
    assert!(open_model_gateway::leases::is_fenced(&refused));
    assert!(refused.to_string().contains("no longer held"), "{refused}");
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT omg_lease_acquire('report_scheduler',$1,3600)"
        )
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap(),
        None
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>("SELECT omg_lease_acquire('report_scheduler',$1,3600) FROM set_config('gateway.report_tzdb','2026e',true)")
            .bind(old).fetch_one(&pool).await.unwrap(), None
    );
    let current = open_model_gateway::leases::Leases::new();
    current.renew_once(&pool).await;
    let new_fence = current
        .held(open_model_gateway::leases::Lease::ReportScheduler)
        .unwrap();
    assert_eq!(new_fence.epoch, epoch + 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM report_runs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT body FROM report_snapshots WHERE run_id=$1"
        )
        .bind(published)
        .fetch_one(&pool)
        .await
        .unwrap(),
        snapshot
    );
}

#[sqlx::test(migrations = false)]
#[allow(clippy::disallowed_methods)] // Dedicated connections for lock ordering.
async fn scheduled_reports_pin_cutover_waits_for_cursor_writer(pool: PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    let id: Uuid = sqlx::query_scalar("WITH creator AS (INSERT INTO users(id,email) VALUES(gen_random_uuid(),'report-cutover@example.test') RETURNING id) INSERT INTO report_schedules(scope_kind,created_by,cadence,timezone,delivery_time) SELECT 'installation',id,'weekly','Cuba','09:00' FROM creator RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let mut writer = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT omg_report_set_cursor($1,1,'2026f','2026-11-23 13:00Z','2026-11-23 00:00Z',NULL)",
    )
    .bind(id)
    .execute(&mut *writer)
    .await
    .unwrap();
    let mut upgrade = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='100ms'")
        .execute(&mut *upgrade)
        .await
        .unwrap();
    let error = sqlx::query("UPDATE work_leases SET report_tzdb='2026g',expires_at='-infinity' WHERE name='report_scheduler'")
        .execute(&mut *upgrade).await.expect_err("cutover interleaved with the cursor transaction");
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55P03")
    );
    upgrade.rollback().await.unwrap();
    writer.commit().await.unwrap();

    let mut upgrade = pool.begin().await.unwrap();
    sqlx::query("UPDATE work_leases SET report_tzdb='2026g',expires_at='-infinity' WHERE name='report_scheduler'")
        .execute(&mut *upgrade).await.unwrap();
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='100ms'")
        .execute(&mut *writer)
        .await
        .unwrap();
    let old_write =
        "SELECT omg_report_set_cursor($1,1,'2026f','2026-11-30 13:00Z','2026-11-30 00:00Z',NULL)";
    let error = sqlx::query(old_write)
        .bind(id)
        .execute(&mut *writer)
        .await
        .expect_err("cursor write interleaved with the pin update");
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55P03")
    );
    writer.rollback().await.unwrap();
    upgrade.commit().await.unwrap();
    let error = sqlx::query(old_write)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap_err();
    let error = error.as_database_error().unwrap();
    assert_eq!(error.code().as_deref(), Some("55006"));
    assert_eq!(error.message(), "report calendar bundle mismatch");
    assert!(sqlx::query_scalar::<_, bool>("SELECT cursor_tzdb='2026f' AND next_run_at='2026-11-23 13:00Z' AND next_period_end='2026-11-23 00:00Z' FROM report_schedules WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap());
}
