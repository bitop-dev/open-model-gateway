//! `ensure_empty` on real migrated databases: a fresh installation passes,
//! every table either seed writes is checked, and each checked table alone
//! makes the check refuse.
use super::*;
use std::collections::BTreeMap;

/// Tables a seed adds rows to only through its writes to `SEEDED_TABLES`
/// (triggers and derived accounting rows). A row in one of these cannot exist
/// without a row in a checked table, so they need no term of their own.
const DERIVED_TABLES: &[(&str, &str)] = &[(
    "configuration_changes",
    "written by the deployments capture trigger (migration 0067) on a deployment insert or update, which also bumps the catalog topic",
)];

/// Row counts of every table in the schema; a partitioned table is counted
/// through its parent.
async fn row_counts(pool: &sqlx::PgPool) -> BTreeMap<String, i64> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace \
         WHERE n.nspname=current_schema() AND c.relkind IN ('r','p') AND NOT c.relispartition",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut counts = BTreeMap::new();
    for table in tables {
        // Names come from the catalog and are quoted as identifiers.
        let quoted = table.replace('"', "\"\"");
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM \"{quoted}\""))
            .fetch_one(pool)
            .await
            .unwrap();
        counts.insert(table, rows);
    }
    counts
}

/// Runs a seed on a freshly migrated database and requires every table that
/// gained rows to be one `ensure_empty` checks or one of `DERIVED_TABLES`, so
/// a new seed write or a migration that adds rows cannot fall outside the
/// check unnoticed.
pub(crate) async fn assert_seed_writes_only_checked_tables(
    pool: &sqlx::PgPool,
    seed: impl std::future::Future<Output = ()>,
) {
    let before = row_counts(pool).await;
    seed.await;
    let after = row_counts(pool).await;
    let unchecked: Vec<&str> = after
        .iter()
        .filter(|(table, rows)| **rows > before.get(*table).copied().unwrap_or(0))
        .map(|(table, _)| table.as_str())
        .filter(|table| {
            !SEEDED_TABLES.contains(table) && !DERIVED_TABLES.iter().any(|(t, _)| t == table)
        })
        .collect();
    assert!(
        unchecked.is_empty(),
        "the seed wrote tables ensure_empty does not check: {unchecked:?}"
    );
}

#[test]
fn derived_tables_are_not_also_checked() {
    for (table, _) in DERIVED_TABLES {
        assert!(!SEEDED_TABLES.contains(table) && !HISTORY_TABLES.contains(table));
    }
}

#[dbtest::test]
async fn dev_seed_writes_only_tables_the_emptiness_check_covers(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    assert_seed_writes_only_checked_tables(&pool, async {
        assert!(
            seed(&store, Environment::Development)
                .await
                .unwrap()
                .is_some()
        );
    })
    .await;
}

/// One row for each checked table. Inserted with triggers and foreign keys
/// off (`session_replication_role=replica`, as a partial restore can leave a
/// table), so no configuration topic moves and no other checked table gains a
/// row: the refusal comes from that table's own term.
const LONE_ROWS: &[(&str, &str)] = &[
    (
        "users",
        "INSERT INTO users(id,email) VALUES(gen_random_uuid(),'lone@example.test')",
    ),
    (
        "platform_role_grants",
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES(gen_random_uuid(),'admin','manual')",
    ),
    (
        "oidc_group_mappings",
        "INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES(gen_random_uuid(),'https://issuer.test','admins','platform','admin')",
    ),
    (
        "cost_centers",
        "INSERT INTO cost_centers(id,name,code) VALUES(gen_random_uuid(),'Lone','LONE')",
    ),
    (
        "workspaces",
        "INSERT INTO workspaces(id,name,kind) VALUES(gen_random_uuid(),'Lone','team')",
    ),
    (
        "workspace_membership_grants",
        "INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES(gen_random_uuid(),gen_random_uuid(),'member','manual')",
    ),
    (
        "service_accounts",
        "INSERT INTO service_accounts(id,workspace_id,name) VALUES(gen_random_uuid(),gen_random_uuid(),'Lone')",
    ),
    (
        "api_keys",
        "INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) SELECT k,gen_random_uuid(),gen_random_uuid(),'Lone',sha256('lone'::bytea),k FROM gen_random_uuid() k",
    ),
    (
        "provider_connections",
        "INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES(gen_random_uuid(),'Lone','openai','env:OPENAI_API_KEY')",
    ),
    (
        "models",
        "INSERT INTO models(id,public_name) VALUES(gen_random_uuid(),'lone/model')",
    ),
    (
        "deployments",
        "INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES(gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),'lone')",
    ),
    (
        "deployment_prices",
        "INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES(gen_random_uuid(),gen_random_uuid(),1,1,1,1,1)",
    ),
    (
        "catalogs",
        "INSERT INTO catalogs(id,name) VALUES(gen_random_uuid(),'Lone')",
    ),
    (
        "catalog_models",
        "INSERT INTO catalog_models(catalog_id,model_id) VALUES(gen_random_uuid(),gen_random_uuid())",
    ),
    (
        "workspace_type_catalogs",
        "INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',gen_random_uuid())",
    ),
    (
        "workspace_catalog_overrides",
        "INSERT INTO workspace_catalog_overrides(workspace_id) VALUES(gen_random_uuid())",
    ),
    (
        "workspace_catalog_override_items",
        "INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES(gen_random_uuid(),gen_random_uuid())",
    ),
    (
        "workspace_model_grants",
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES(gen_random_uuid(),gen_random_uuid(),'direct')",
    ),
    (
        "policy_budgets",
        "INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','team','month',1)",
    ),
    (
        "audit_events",
        "INSERT INTO audit_events(id,action,resource_type) VALUES(gen_random_uuid(),'user.bootstrap_role','user')",
    ),
    (
        "inference_executions",
        "INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,workload_kind,started_at,root_request_id,attempt_number) SELECT k,gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),'lone/model','openai',false,'succeeded','generation',now(),k,1 FROM gen_random_uuid() k",
    ),
];

async fn as_replica(pool: &sqlx::PgPool, statement: &str) {
    let mut tx = crate::db::begin(pool).await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(statement)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"));
    tx.commit().await.unwrap();
}

async fn empty(pool: &sqlx::PgPool) -> Result<()> {
    let mut tx = crate::db::begin(pool).await.unwrap();
    ensure_empty(&mut tx).await
}

#[dbtest::test]
async fn a_row_in_any_checked_table_alone_is_refused(pool: sqlx::PgPool) {
    let mut listed: Vec<&str> = LONE_ROWS.iter().map(|(table, _)| *table).collect();
    let mut checked: Vec<&str> = SEEDED_TABLES
        .iter()
        .chain(HISTORY_TABLES)
        .copied()
        .collect();
    listed.sort_unstable();
    checked.sort_unstable();
    assert_eq!(listed, checked, "every checked table has a refusal case");
    empty(&pool).await.unwrap();
    for (table, insert) in LONE_ROWS {
        as_replica(&pool, insert).await;
        let counts = row_counts(&pool).await;
        let occupied: Vec<&str> = checked.iter().copied().filter(|t| counts[*t] > 0).collect();
        assert_eq!(occupied, [*table]);
        let changed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM config_versions WHERE version>0)")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!changed, "{table}");
        let refused = empty(&pool).await.unwrap_err().to_string();
        assert!(refused.contains("requires an empty"), "{table}: {refused}");
        as_replica(&pool, &format!("DELETE FROM {table}")).await;
        empty(&pool).await.unwrap();
    }
}

/// Rows `seed` would write next to, or configuration changes, mean the
/// installation is in use: nothing is seeded and no admin is created.
async fn assert_refused(pool: &sqlx::PgPool, what: &str) {
    let store = Store::new(pool.clone());
    let error = seed(&store, Environment::Development).await.err();
    assert!(
        error.is_some_and(|e| e.to_string().contains("requires an empty")),
        "{what}"
    );
    let written: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM platform_role_grants),(SELECT count(*) FROM api_keys),(SELECT count(*) FROM users WHERE email='developer@local.invalid')").fetch_one(pool).await.unwrap();
    assert_eq!(written, (0, 0, 0), "{what}");
}
#[dbtest::test]
async fn dev_bootstrap_refuses_a_catalog_only_installation(pool: sqlx::PgPool) {
    let connection = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Existing','openai','env:OPENAI_API_KEY')")
        .bind(connection).execute(&pool).await.unwrap();
    assert_refused(&pool, "a provider connection").await;
    sqlx::query("DELETE FROM provider_connections")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'existing/model')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    assert_refused(&pool, "a model").await;
}
#[dbtest::test]
async fn dev_bootstrap_refuses_a_changed_configuration(pool: sqlx::PgPool) {
    sqlx::query("UPDATE installation_settings SET support_url='https://support.example.test'")
        .execute(&pool)
        .await
        .unwrap();
    assert_refused(&pool, "changed installation settings").await;
}
#[dbtest::test]
async fn dev_bootstrap_refuses_an_installation_with_only_audit_history(pool: sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO audit_events(id,action,resource_type) VALUES($1,'user.bootstrap_role','user')",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    assert_refused(&pool, "audit history").await;
}
#[dbtest::test]
async fn dev_bootstrap_refuses_an_installation_with_only_executions(pool: sqlx::PgPool) {
    // History whose keys and workspaces are gone, as after a partial
    // restore: the parent and accounting triggers are off for the insert.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    sqlx::query("ALTER TABLE inference_executions DISABLE TRIGGER USER")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,workload_kind,started_at,root_request_id,attempt_number) VALUES($1,$2,$3,$4,'restored/model','openai',false,'succeeded','generation',now(),$1,1)")
        .bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE inference_executions ENABLE TRIGGER USER")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let occupied: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM workspaces),(SELECT count(*) FROM api_keys)").fetch_one(&pool).await.unwrap();
    assert_eq!(occupied, (1, 0, 0));
    assert_refused(&pool, "execution history").await;
}
/// A freshly migrated installation is empty, and a repeat run finds the
/// installation's own bootstrap marker before checking emptiness.
#[dbtest::test]
async fn dev_bootstrap_seeds_a_fresh_installation_and_repeats_without_changes(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    assert!(
        seed(&store, Environment::Development)
            .await
            .unwrap()
            .is_some()
    );
    let changed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM config_versions WHERE version>0)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        changed,
        "the seed's own catalog rows bump configuration versions"
    );
    assert!(
        seed(&store, Environment::Development)
            .await
            .unwrap()
            .is_none()
    );
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(keys, 2);
}
