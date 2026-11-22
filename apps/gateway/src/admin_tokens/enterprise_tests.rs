//! #37 PR1 real-PostgreSQL coverage: schema constraints, immutability and
//! no-delete, revocation finality, owner transitions, lookup-for-
//! authorization, the parser's no-DB-access refusal and the last-use
//! throttle. Runtime-role ACL probes live in `tests/runtime_privileges.rs`.
use super::*;
use gateway_datetime_compat::UtcInstant;
use sqlx::PgPool;

/// Test convenience: `create_admin_token` on a transaction of its own
/// (begin, call, commit/rollback). Most tests below don't care about the
/// lock-holding contract PR2 must follow (see the dedicated tests that
/// do, further down); they just want a token.
async fn create(
    pool: &PgPool,
    owner: Uuid,
    name: &str,
    scopes: &[String],
    days: i32,
    allowlist: Option<&[String]>,
) -> Result<NewAdminToken, CreateAdminTokenError> {
    let mut tx = crate::db::begin(pool).await.unwrap();
    // The creation guard requires the owner's exclusive user-scope lock
    // before the insert, held by the same transaction.
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    match create_admin_token(&mut tx, owner, name, scopes, days, allowlist).await {
        Ok(token) => {
            tx.commit().await.unwrap();
            Ok(token)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Test convenience: `revoke_admin_token` on a transaction of its own.
async fn revoke(pool: &PgPool, id: Uuid) -> bool {
    let mut tx = crate::db::begin(pool).await.unwrap();
    let result = revoke_admin_token(&mut tx, id).await.unwrap();
    tx.commit().await.unwrap();
    result
}

async fn admin(pool: &PgPool, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

fn scopes(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[dbtest::test]
async fn create_returns_plaintext_once_and_authenticate_round_trips(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = create(
        &pool,
        owner,
        "CI token",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    assert!(created.token.starts_with("omg_admin_"));
    let found = store
        .authenticate_admin_token(&created.token)
        .await
        .unwrap()
        .expect("a live token authenticates");
    assert_eq!(found.token_id, created.id);
    assert_eq!(found.owner_user_id, owner);
    assert_eq!(found.scopes, vec!["platform:read".to_string()]);
    // The stored row never carries the plaintext or anything derived from it
    // other than the digest.
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT secret_hash FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, created.digest.to_vec());
    // Change the last hex digit to guarantee a different secret while
    // keeping the token ID intact.
    let last = created.token.chars().next_back().unwrap();
    let flipped = if last == '0' { '1' } else { '0' };
    let tampered = format!("{}{flipped}", &created.token[..created.token.len() - 1]);
    assert!(
        store
            .authenticate_admin_token(&tampered)
            .await
            .unwrap()
            .is_none()
    );
}

#[dbtest::test]
async fn create_rejects_invalid_input_before_touching_the_database(pool: PgPool) {
    let owner = admin(&pool, "owner@test.invalid").await;
    assert!(matches!(
        create(&pool, owner, "", &scopes(&["platform:read"]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName
        ))
    ));
    assert!(matches!(
        create(
            &pool,
            owner,
            &"x".repeat(121),
            &scopes(&["platform:read"]),
            30,
            None
        )
        .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName
        ))
    ));
    assert!(matches!(
        create(&pool, owner, "n", &scopes(&[]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        create(&pool, owner, "n", &scopes(&["platform:admin"]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read", "platform:read"]),
            30,
            None
        )
        .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    for days in [0, 91, -1] {
        assert!(matches!(
            create(&pool, owner, "n", &scopes(&["platform:read"]), days, None).await,
            Err(CreateAdminTokenError::Invalid(
                NewAdminTokenError::InvalidExpiry
            ))
        ));
    }
    assert!(matches!(
        create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&[])
        )
        .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidAllowlist
        ))
    ));
    let too_many: Vec<String> = (0..33).map(|i| format!("10.0.{i}.0/24")).collect();
    assert!(matches!(
        create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&too_many)
        )
        .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidAllowlist
        ))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for a refused creation"
    );
}

#[dbtest::test]
async fn create_accepts_a_valid_cidr_allowlist(pool: PgPool) {
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
    .await
    .unwrap();
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT ip_allowlist::text[] FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 2);
}

/// Malformed CIDRs return the typed `InvalidAllowlist` error before a
/// database call. This covers invalid addresses and prefix lengths,
/// nonzero host bits, and IPv4-mapped IPv6 networks.
#[dbtest::test]
async fn create_rejects_malformed_allowlist_entries_before_touching_the_database(pool: PgPool) {
    let owner = admin(&pool, "cidr@test.invalid").await;
    for bad in [
        "not-an-ip",
        "10.0.0.1/33",
        "10.0.0.5/24",
        "2001:db8::/129",
        "::ffff:10.0.0.0/120",
    ] {
        let result = create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&[bad.to_string()]),
        )
        .await;
        // NewAdminToken deliberately carries no Debug (it holds the
        // plaintext token), so match on the error shape only.
        assert!(
            matches!(
                result,
                Err(CreateAdminTokenError::Invalid(
                    NewAdminTokenError::InvalidAllowlist
                ))
            ),
            "{bad:?} should be InvalidAllowlist"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for any of these malformed entries"
    );
}

/// Every stored entry is the canonical `addr/len` form, not whatever
/// capitalization/representation the caller happened to send.
#[dbtest::test]
async fn create_canonicalizes_allowlist_entries(pool: PgPool) {
    let owner = admin(&pool, "cidr-canon@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:DB8::/32".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
    .await
    .unwrap();
    let mut stored: Vec<String> =
        sqlx::query_scalar("SELECT ip_allowlist::text[] FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    stored.sort();
    assert_eq!(
        stored,
        vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()]
    );
}

#[dbtest::test]
async fn database_level_schema_constraints_hold_even_bypassing_the_rust_validator(pool: PgPool) {
    // These exercise migration 0051's CHECK constraints directly: the Rust
    // validator in create_admin_token is a fail-fast gate, not the only one.
    let owner = admin(&pool, "owner@test.invalid").await;
    let make_hash = |b: u8| vec![b; 32];
    let insert = |scopes_sql: &str, expires_sql: &str, allowlist_sql: Option<&str>| {
        format!(
            "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,ip_allowlist,expires_at)
             VALUES(gen_random_uuid(),'{owner}','probe',decode('{hash}','hex'),{scopes_sql},{allowlist},now()+{expires_sql})",
            hash = hex::encode(make_hash(0x11)),
            allowlist = allowlist_sql.unwrap_or("NULL")
        )
    };
    for (label, sql) in [
        (
            "unknown scope",
            insert("ARRAY['wizard:cast']", "interval '10 days'", None),
        ),
        (
            "empty scopes",
            insert("ARRAY[]::text[]", "interval '10 days'", None),
        ),
        (
            "duplicate scopes",
            insert(
                "ARRAY['platform:read','platform:read']",
                "interval '10 days'",
                None,
            ),
        ),
        (
            "expiry too short",
            insert("ARRAY['platform:read']", "interval '12 hours'", None),
        ),
        (
            "expiry too long",
            insert("ARRAY['platform:read']", "interval '91 days'", None),
        ),
        (
            "empty allowlist",
            insert(
                "ARRAY['platform:read']",
                "interval '10 days'",
                Some("ARRAY[]::cidr[]"),
            ),
        ),
        // Include a zero-length prefix and a duplicate entry in the direct SQL probes.
        (
            "zero-length prefix allowlist",
            insert(
                "ARRAY['platform:read']",
                "interval '10 days'",
                Some("ARRAY['0.0.0.0/0']::cidr[]"),
            ),
        ),
        (
            "duplicate allowlist entry",
            insert(
                "ARRAY['platform:read']",
                "interval '10 days'",
                Some("ARRAY['10.0.0.0/24','10.0.0.0/24']::cidr[]"),
            ),
        ),
    ] {
        // These probes omit the owner's scope lock, so the creation guard can
        // reject the insert before the schema CHECK runs. The loop asserts
        // that the insert fails.
        let result = sqlx::query(&sql).execute(&pool).await;
        assert!(
            result.is_err(),
            "{label} should be rejected by the database"
        );
    }
    // 33 allowlist entries, built with generate_series (cardinality bound).
    let too_many = "(SELECT array_agg(('10.0.'||g||'.0/24')::cidr) FROM generate_series(1,33) g)";
    let sql = insert(
        "ARRAY['platform:read']",
        "interval '10 days'",
        Some(too_many),
    );
    assert!(
        sqlx::query(&sql).execute(&pool).await.is_err(),
        "33 allowlist entries should be rejected"
    );
    // A fully valid row (the positive control; schema constraints did not
    // over-reject anything else). This one must succeed, so it needs the
    // owner's scope lock held first.
    let sql = insert("ARRAY['platform:read']", "interval '10 days'", None);
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    sqlx::query(&sql).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

#[dbtest::test]
async fn identity_digest_scopes_expiry_and_allowlist_are_immutable_and_rows_are_never_deleted(
    pool: PgPool,
) {
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
    .await
    .unwrap();
    for (label, sql) in [
        (
            "name",
            "UPDATE admin_api_tokens SET name='renamed' WHERE id=$1",
        ),
        (
            "owner",
            "UPDATE admin_api_tokens SET owner_user_id=gen_random_uuid() WHERE id=$1",
        ),
        (
            "digest",
            "UPDATE admin_api_tokens SET secret_hash=decode(repeat('99',32),'hex') WHERE id=$1",
        ),
        (
            "scopes",
            "UPDATE admin_api_tokens SET scopes=ARRAY['settings:write'] WHERE id=$1",
        ),
        (
            "expiry",
            "UPDATE admin_api_tokens SET expires_at=now()+interval '1 day' WHERE id=$1",
        ),
        (
            "allowlist",
            "UPDATE admin_api_tokens SET ip_allowlist=ARRAY['1.1.1.0/24']::cidr[] WHERE id=$1",
        ),
    ] {
        let result = sqlx::query(sql).bind(created.id).execute(&pool).await;
        assert!(result.is_err(), "{label} must be immutable");
    }
    assert!(
        sqlx::query("DELETE FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .execute(&pool)
            .await
            .is_err(),
        "no row may ever be deleted"
    );
    assert!(
        sqlx::query("TRUNCATE admin_api_tokens")
            .execute(&pool)
            .await
            .is_err(),
        "the table may never be truncated"
    );
}

#[dbtest::test]
async fn revocation_is_final_and_revoke_is_idempotent(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(revoke(&pool, created.id).await);
    // Idempotent: revoking again is not an error and does not move the time.
    let first: UtcInstant =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoke(&pool, created.id).await);
    let second: UtcInstant =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first, second);
    // Revoking a token that never existed is not an error either.
    assert!(!revoke(&pool, Uuid::new_v4()).await);
    // Cannot be un-revoked, by anyone, ever.
    assert!(
        sqlx::query("UPDATE admin_api_tokens SET revoked_at=NULL WHERE id=$1")
            .bind(created.id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        store
            .authenticate_admin_token(&created.token)
            .await
            .unwrap()
            .is_none(),
        "a revoked token never authenticates"
    );
}

#[dbtest::test]
async fn lookup_rejects_expired_revoked_and_owner_not_admin(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;

    // Seed the expired fixture by disabling only the creation guard, which
    // sets `created_at` to the current database time. The historical
    // timestamps still have to satisfy the schema CHECK.
    let raw = GeneratedSecret::generate().unwrap();
    // One transaction: a panic rolls the DISABLE back instead of leaving
    // the guard off for the rest of this test database.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE admin_api_tokens DISABLE TRIGGER admin_api_tokens_owner_is_admin")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,created_at,expires_at) VALUES($1,$2,'expired',$3,ARRAY['platform:read'],now()-interval '10 days',now()-interval '1 minute')")
        .bind(raw.id)
        .bind(owner)
        .bind(raw.digest.as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE admin_api_tokens ENABLE TRIGGER admin_api_tokens_owner_is_admin")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        store
            .authenticate_admin_token(&raw.token)
            .await
            .unwrap()
            .is_none(),
        "expired tokens never authenticate"
    );

    let revoked = create(
        &pool,
        owner,
        "revoked",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    assert!(revoke(&pool, revoked.id).await);
    assert!(
        store
            .authenticate_admin_token(&revoked.token)
            .await
            .unwrap()
            .is_none()
    );

    // Owner demoted to a non-admin role: the token no longer authenticates,
    // even though it is itself unrevoked and unexpired.
    let demoted = create(
        &pool,
        owner,
        "demoted",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    // Migration 0051's trigger already revoked it on the role-loss UPDATE
    // above (owner-invalidation); the lookup refuses it either way.
    assert!(
        store
            .authenticate_admin_token(&demoted.token)
            .await
            .unwrap()
            .is_none()
    );
    let revoked_by_trigger: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(demoted.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        revoked_by_trigger,
        "losing the last admin grant revokes owned tokens immediately"
    );
}

#[dbtest::test]
async fn owner_transitions_revoke_tokens_and_regaining_admin_never_restores_them(pool: PgPool) {
    // Manual demotion of the only admin grant.
    let manual = admin(&pool, "manual@test.invalid").await;
    let t1 = create(&pool, manual, "t1", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(manual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(revoked(&pool, t1.id).await);
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')",
    )
    .bind(Uuid::new_v4())
    .bind(manual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        revoked(&pool, t1.id).await,
        "regaining admin after manual demotion does not restore the token"
    );

    // Group-derived admin removal.
    let grouped = admin(&pool, "grouped@test.invalid").await; // seeds a manual grant too
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(grouped)
        .execute(&pool)
        .await
        .unwrap(); // start from zero grants
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(grouped)
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let t2 = create(&pool, grouped, "t2", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='group'",
    )
    .bind(grouped)
    .execute(&pool)
    .await
    .unwrap();
    assert!(revoked(&pool, t2.id).await);

    // Suspension.
    let suspended = admin(&pool, "suspended@test.invalid").await;
    let t3 = create(
        &pool,
        suspended,
        "t3",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(suspended)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t3.id).await);
    sqlx::query("UPDATE users SET disabled_at=NULL,disable_reason=NULL WHERE id=$1")
        .bind(suspended)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, t3.id).await,
        "reinstating a suspended owner does not restore the token"
    );

    // SCIM deactivation uses the same disabled_at column (scim.rs).
    let scim_deactivated = admin(&pool, "scim@test.invalid").await;
    let t4 = create(
        &pool,
        scim_deactivated,
        "t4",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='scim_deactivated' WHERE id=$1")
        .bind(scim_deactivated)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t4.id).await);

    // Cleanup.
    let cleaned = admin(&pool, "cleaned@test.invalid").await;
    let t5 = create(&pool, cleaned, "t5", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(cleaned)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t5.id).await);

    // Two active admin grants: revoking only one must not revoke the token.
    let dual = admin(&pool, "dual@test.invalid").await; // manual grant #1
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','bootstrap')")
        .bind(Uuid::new_v4())
        .bind(dual)
        .execute(&pool)
        .await
        .unwrap();
    let t6 = create(&pool, dual, "t6", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='manual'",
    )
    .bind(dual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !revoked(&pool, t6.id).await,
        "one of two remaining admin grants keeps the token active"
    );
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='bootstrap'",
    )
    .bind(dual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        revoked(&pool, t6.id).await,
        "losing the last remaining admin grant revokes the token"
    );
}

/// Concurrent revocations of manual and group Admin grants must revoke
/// the token permanently. Under READ COMMITTED, each transaction could
/// otherwise see the other's uncommitted grant as still active.
///
/// Disable the separate migration 0027 grants trigger, whose advisory
/// lock would serialize the revocations. This isolates the migration 0051
/// role-loss trigger's own concurrency guarantee.
async fn write_skew_scenario(pool: &PgPool, first_source: &str) {
    let second_source = if first_source == "manual" {
        "group"
    } else {
        "manual"
    };
    let owner = admin(pool, &format!("writeskew-{first_source}@test.invalid")).await;
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(mapping)
    .execute(pool)
    .await
    .unwrap();
    let token = create(pool, owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    sqlx::query("ALTER TABLE platform_role_grants DISABLE TRIGGER omg_authority_platform_grants")
        .execute(pool)
        .await
        .unwrap();

    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query(&format!(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='{first_source}'"
    ))
    .bind(owner)
    .execute(&mut *tx1)
    .await
    .unwrap();

    let pool2 = pool.clone();
    let second_sql = format!(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='{second_source}'"
    );
    let handle = tokio::spawn(async move {
        let mut tx2 = pool2.begin().await.unwrap();
        sqlx::query(&second_sql)
            .bind(owner)
            .execute(&mut *tx2)
            .await
            .unwrap();
        tx2.commit().await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let blocked_while_first_open = !handle.is_finished();
    tx1.commit().await.unwrap();
    handle.await.unwrap();

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        revoked,
        "two admin grants revoked by concurrent, unsynchronized writers (second blocked while first was open={blocked_while_first_open}) must still revoke the token"
    );
}

#[dbtest::test]
async fn concurrent_revocation_of_manual_then_group_admin_grants_still_revokes_the_token(
    pool: PgPool,
) {
    write_skew_scenario(&pool, "manual").await;
}

#[dbtest::test]
async fn concurrent_revocation_of_group_then_manual_admin_grants_still_revokes_the_token(
    pool: PgPool,
) {
    write_skew_scenario(&pool, "group").await;
}

fn assert_owner_not_admin<T>(result: &Result<T, CreateAdminTokenError>) {
    match result {
        Err(CreateAdminTokenError::OwnerNotAdmin) => {}
        Err(other) => panic!("expected OwnerNotAdmin, got a different error: {other:?}"),
        Ok(_) => panic!("expected OwnerNotAdmin, got Ok"),
    }
}

/// Creation requires live Admin status at insert time. A grant lost after
/// an earlier authorization check must not leave a token that becomes
/// usable on a later regrant.
#[dbtest::test]
async fn create_fails_when_the_owner_is_not_currently_an_effective_admin(pool: PgPool) {
    let non_admin = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(non_admin)
        .bind("plain@test.invalid")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')",
    )
    .bind(Uuid::new_v4())
    .bind(non_admin)
    .execute(&pool)
    .await
    .unwrap();
    let result = create(&pool, non_admin, "n", &scopes(&["platform:read"]), 30, None).await;
    assert_owner_not_admin(&result);

    let disabled = admin(&pool, "disabled-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    let result = create(&pool, disabled, "n", &scopes(&["platform:read"]), 30, None).await;
    assert_owner_not_admin(&result);

    let cleaned = admin(&pool, "cleaned-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(cleaned)
        .execute(&pool)
        .await
        .unwrap();
    let result = create(&pool, cleaned, "n", &scopes(&["platform:read"]), 30, None).await;
    assert_owner_not_admin(&result);

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for any of these refused owners"
    );
}

/// A concurrent demotion racing a concurrent creation: the create must
/// either see the owner still admin (and succeed, atomically with respect
/// to the demotion) or block until the demotion commits and then fail,
/// never insert a token for an owner who has already lost admin.
#[dbtest::test]
async fn concurrent_demotion_blocks_and_then_fails_a_pending_create(pool: PgPool) {
    let owner = admin(&pool, "race-create@test.invalid").await;

    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx1)
    .await
    .unwrap();
    // tx1's role-loss trigger now holds the owner's user-scope advisory lock.

    let pool2 = pool.clone();
    let handle = tokio::spawn(async move {
        // PR2's own contract: take the lock up front in the same
        // transaction, then create on it.
        let mut tx2 = crate::db::begin(&pool2).await.unwrap();
        crate::governance::locks::exclusive(
            &mut tx2,
            [crate::governance::locks::Scope::User(owner)],
        )
        .await
        .unwrap();
        let result = create_admin_token(
            &mut tx2,
            owner,
            "race",
            &["platform:read".to_string()],
            30,
            None,
        )
        .await;
        match &result {
            Ok(_) => tx2.commit().await.unwrap(),
            Err(_) => tx2.rollback().await.unwrap(),
        }
        result
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let blocked_while_demotion_open = !handle.is_finished();
    tx1.commit().await.unwrap();
    let result = handle.await.unwrap();
    if !matches!(result, Err(CreateAdminTokenError::OwnerNotAdmin)) {
        panic!("blocked_while_demotion_open={blocked_while_demotion_open}, expected OwnerNotAdmin");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_api_tokens WHERE owner_user_id=$1 AND revoked_at IS NULL",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "no active token exists for the now-demoted owner, blocked_while_demotion_open={blocked_while_demotion_open}"
    );
}

/// Creation and expiration use the same database instant. App clock skew
/// must not change the token's lifetime.
#[dbtest::test]
async fn created_at_and_expires_at_come_from_the_database_clock_not_the_app(pool: PgPool) {
    let owner = admin(&pool, "clock@test.invalid").await;
    let before: UtcInstant = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    let after: UtcInstant = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        created.created_at >= before - gateway_datetime_compat::SignedDuration::seconds(1),
        "created_at={:?} before={before:?}",
        created.created_at
    );
    assert!(
        created.created_at <= after + gateway_datetime_compat::SignedDuration::seconds(1),
        "created_at={:?} after={after:?}",
        created.created_at
    );
    assert_eq!(
        created.expires_at - created.created_at,
        gateway_datetime_compat::SignedDuration::days(30)
    );
    // The returned timestamps are exactly what the row stores, not a
    // separately-computed app-side echo of them.
    let (db_created, db_expires): (UtcInstant, UtcInstant) =
        sqlx::query_as("SELECT created_at,expires_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (db_created, db_expires),
        (created.created_at, created.expires_at)
    );
}

fn is_deadlock(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("40P01"))
}

/// Concurrent manual and group revocation finish without a deadlock.
/// Taking the user-scope advisory lock before mutating grants avoids a
/// cycle between a transaction holding the user row `FOR UPDATE` and one
/// holding the advisory lock.
#[dbtest::test]
async fn concurrent_manual_and_group_revocation_does_not_deadlock(pool: PgPool) {
    let owner = admin(&pool, "deadlock@test.invalid").await;
    let manual_grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM platform_role_grants WHERE user_id=$1 AND role='admin' AND source='manual'",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let group_grant = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)",
    )
    .bind(group_grant)
    .bind(owner)
    .bind(mapping)
    .execute(&pool)
    .await
    .unwrap();
    let token = create(&pool, owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    // T1: the manual demotion path's first step (directory.rs).
    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut *tx1)
        .await
        .unwrap();

    // T2: the group-mapping deletion's grant revoke (directory.rs), spawned.
    let pool2 = pool.clone();
    let t2 = tokio::spawn(async move {
        let mut tx2 = pool2.begin().await.unwrap();
        let result = sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE id=$1")
            .bind(group_grant)
            .execute(&mut *tx2)
            .await;
        match result {
            Ok(_) => {
                tx2.commit().await.unwrap();
                Ok(())
            }
            Err(e) => {
                let _ = tx2.rollback().await;
                Err(e)
            }
        }
    });

    // Let T2 reach its blocking point (unfixed) or finish entirely (fixed).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // T1: the manual demotion's own grant revoke.
    let result1 = sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE id=$1")
        .bind(manual_grant)
        .execute(&mut *tx1)
        .await;
    let outcome1 = match result1 {
        Ok(_) => {
            tx1.commit().await.unwrap();
            Ok(())
        }
        Err(e) => {
            let _ = tx1.rollback().await;
            Err(e)
        }
    };
    let outcome2 = t2.await.unwrap();

    for (label, outcome) in [("T1 (manual)", &outcome1), ("T2 (group)", &outcome2)] {
        if let Err(e) = outcome {
            assert!(!is_deadlock(e), "{label} hit SQLSTATE 40P01: {e}");
        }
    }
    assert!(
        outcome1.is_ok() || outcome2.is_ok(),
        "{outcome1:?} / {outcome2:?}"
    );

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        revoked,
        "losing both admin grants must revoke the token even when neither side deadlocked"
    );
}

/// Manual demotion can reacquire the already-held user-scope lock from
/// the token role-loss trigger while scope-lock auditing is active.
/// Advisory locks are reentrant within the transaction.
#[dbtest::test]
async fn manual_demotion_under_scope_lock_audit_mode_does_not_trip_the_audit_check(pool: PgPool) {
    let owner = admin(&pool, "audit@test.invalid").await;
    let token = create(&pool, owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked);
}

async fn revoked(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn last_use_is_throttled_to_once_per_five_minutes(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    store
        .touch_admin_token_use(created.id, Some("203.0.113.5".parse().unwrap()))
        .await
        .unwrap();
    let (first_at, first_addr): (UtcInstant, String) = sqlx::query_as(
        "SELECT last_used_at,host(last_used_address) FROM admin_api_tokens WHERE id=$1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first_addr, "203.0.113.5");
    // A second use moments later, with a different address, does not write.
    store
        .touch_admin_token_use(created.id, Some("198.51.100.9".parse().unwrap()))
        .await
        .unwrap();
    let (second_at, second_addr): (UtcInstant, String) = sqlx::query_as(
        "SELECT last_used_at,host(last_used_address) FROM admin_api_tokens WHERE id=$1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (second_at, second_addr),
        (first_at, "203.0.113.5".to_string())
    );
    // Once the window has passed, the next use writes again.
    sqlx::query("UPDATE admin_api_tokens SET last_used_at=now()-interval '6 minutes' WHERE id=$1")
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap();
    store
        .touch_admin_token_use(created.id, Some("198.51.100.9".parse().unwrap()))
        .await
        .unwrap();
    let third_addr: String =
        sqlx::query_scalar("SELECT host(last_used_address) FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(third_addr, "198.51.100.9");
}

#[dbtest::test]
async fn malformed_tokens_never_reach_the_database(pool: PgPool) {
    let store = Store::new(pool.clone());
    pool.close().await;
    // A closed pool would error on any query; a structurally invalid token
    // must still return `Ok(None)` because the parser refuses it first.
    for bad in ["", "omg_admin_not-a-token", "omg_ ", "Bearer x"] {
        assert!(store.authenticate_admin_token(bad).await.unwrap().is_none());
    }
}

#[dbtest::test]
async fn owner_restrict_prevents_removing_a_user_with_live_tokens(pool: PgPool) {
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(owner)
            .execute(&pool)
            .await
            .is_err(),
        "owner FK is RESTRICT: a referenced user cannot be removed"
    );
    let _ = created.digest; // silence unused-field lints on this struct's other field
}

/// The creation guard refuses REPEATABLE READ and SERIALIZABLE snapshots
/// that still see a grant after another transaction has revoked it.
/// The user-scope lock serializes writes but cannot refresh an established
/// snapshot. Raw SQL lets the test keep that snapshot on a single connection.
async fn isolation_guard_refuses_a_stale_snapshot(pool: &PgPool, setup: &'static str, label: &str) {
    let owner = admin(pool, &format!("stale-{label}@test.invalid")).await;
    let mut tx = crate::db::begin_with_setup(pool, setup).await.unwrap();
    // Deliberately not taking `governance::locks::exclusive` here (doing
    // so before the concurrent revoke below commits would deadlock: that
    // revoke's own trigger, 0027's omg_authority_platform_grants, takes
    // the very same lock, and this tx would already be holding it). That
    // is fine for this test either way: under RR/SERIALIZABLE the
    // creation guard's isolation check (`OM002`) fires before its
    // lock-holding check (`OM003`) ever runs, so this stale-snapshot
    // `INSERT` below still fails regardless of whether the lock is held.
    // Establish the snapshot: observe the owner as currently effective admin.
    let seen_admin: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=$1 AND role='admin')",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(
        seen_admin,
        "the snapshot must see the owner as admin before the concurrent revoke"
    );
    // A concurrent transaction revokes the owner's only admin grant and commits.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    // The stale snapshot still attempts to create a token for this now-demoted owner.
    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,expires_at)
         VALUES($1,$2,'stale',decode(repeat('11',32),'hex'),ARRAY['platform:read'],now()+interval '30 days')",
    )
    .bind(id)
    .bind(owner)
    .execute(&mut *tx)
    .await;
    let _ = tx.rollback().await;
    assert!(
        result.is_err(),
        "a {label} snapshot predating a committed demotion must not create a token for the now-demoted owner"
    );
}

#[dbtest::test]
async fn creation_guard_refuses_a_stale_repeatable_read_snapshot(pool: PgPool) {
    isolation_guard_refuses_a_stale_snapshot(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "rr",
    )
    .await;
}

#[dbtest::test]
async fn creation_guard_refuses_a_stale_serializable_snapshot(pool: PgPool) {
    isolation_guard_refuses_a_stale_snapshot(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "serializable",
    )
    .await;
}

/// The common case (the gateway's own isolation level) is unaffected: a
/// plain READ COMMITTED creation still works.
#[dbtest::test]
async fn creation_guard_allows_read_committed(pool: PgPool) {
    let owner = admin(&pool, "read-committed@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,expires_at)
         VALUES($1,$2,'rc',decode(repeat('11',32),'hex'),ARRAY['platform:read'],now()+interval '30 days')",
    )
    .bind(id)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// The role-loss check (`omg_admin_tokens_role_lost`) has the same
/// snapshot problem in principle (it also does a fresh `EXISTS` after
/// taking the lock): refuse it under RR/SERIALIZABLE too, even though no
/// production code path runs a `platform_role_grants` write at an elevated
/// isolation level today (verified by grep; this is pure defense in depth).
async fn role_loss_guard_refuses_elevated_isolation(
    pool: &PgPool,
    setup: &'static str,
    label: &str,
) {
    let owner = admin(pool, &format!("role-loss-stale-{label}@test.invalid")).await;
    let mut tx = crate::db::begin_with_setup(pool, setup).await.unwrap();
    // The deferred role-loss trigger reports its refusal at commit.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    let result = tx.commit().await;
    assert!(
        result.is_err(),
        "the role-loss guard must refuse to run under a {label} isolation level"
    );
}

#[dbtest::test]
async fn role_loss_guard_refuses_repeatable_read(pool: PgPool) {
    role_loss_guard_refuses_elevated_isolation(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "rr",
    )
    .await;
}

#[dbtest::test]
async fn role_loss_guard_refuses_serializable(pool: PgPool) {
    role_loss_guard_refuses_elevated_isolation(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "serializable",
    )
    .await;
}

#[dbtest::test]
async fn role_loss_guard_allows_read_committed(pool: PgPool) {
    let owner = admin(&pool, "role-loss-rc@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Creation completes on the caller's transaction when it already holds
/// the owner's exclusive user-scope lock. A separate connection would
/// wait for the caller's lock while the caller waits for creation.
#[dbtest::test]
async fn create_must_not_hang_when_the_caller_already_holds_the_owners_scope_lock(pool: PgPool) {
    let owner = admin(&pool, "hang-create@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        create_admin_token(&mut tx, owner, "n", &scopes(&["platform:read"]), 30, None),
    )
    .await;
    let created = result
        .expect("create_admin_token hung while the caller's own transaction already held the owner's scope lock (PR2's documented contract)")
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!revoked(&pool, created.id).await);
}

/// Token revocation shares the caller's transaction so it rolls back with
/// the other writes, such as the audit event.
#[dbtest::test]
async fn revoke_runs_on_the_callers_transaction_and_rolls_back_with_it(pool: PgPool) {
    let owner = admin(&pool, "revoke-tx@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert!(revoke_admin_token(&mut tx, token.id).await.unwrap());
    tx.rollback().await.unwrap();
    assert!(
        !revoked(&pool, token.id).await,
        "revoke_admin_token must run on the caller's own transaction, so rolling it back undoes the revocation"
    );
    // The converse: committing the caller's transaction commits the revoke.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert!(revoke_admin_token(&mut tx, token.id).await.unwrap());
    tx.commit().await.unwrap();
    assert!(revoked(&pool, token.id).await);
}

/// Replacing a group-sourced Admin grant within one transaction preserves
/// the token. The role-loss trigger evaluates the final grant set at
/// commit, after the stale grant has been revoked and its replacement
/// inserted.
#[dbtest::test]
async fn a_group_mapping_recreation_that_replaces_the_grant_does_not_destroy_the_token(
    pool: PgPool,
) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'group-resync@test.invalid')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let old_mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    // The operator recreates the mapping: same group value and role, a new id.
    sqlx::query("DELETE FROM oidc_group_mappings WHERE id=$1")
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    let new_mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(new_mapping)
        .execute(&pool)
        .await
        .unwrap();

    // lifecycle.rs's exact two-statement replacement, replayed in one transaction.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::apply_group_grants(
        &mut tx,
        owner,
        "https://issuer.test",
        &["admins".to_string()],
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert!(
        !revoked(&pool, token.id).await,
        "a group mapping recreation that replaces the grant in the same transaction must not revoke the owner's tokens"
    );
}

/// Companion to the above: a real group grant loss with no replacement
/// (the user's claimed groups no longer include the admin group at all)
/// must still revoke the owner's tokens, once the transaction commits.
#[dbtest::test]
async fn a_real_group_grant_loss_with_no_replacement_still_revokes_at_commit(pool: PgPool) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'group-loss@test.invalid')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::apply_group_grants(&mut tx, owner, "https://issuer.test", &[], None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(
        revoked(&pool, token.id).await,
        "losing the group grant with no replacement must still revoke the owner's tokens, at commit"
    );
}

/// Changing the Admin grant's role, deleting it, or moving it to a different
/// user revokes the original owner's tokens when they lose Admin. These
/// tests use the migrator role to exercise mutations the runtime cannot
/// perform directly.
#[dbtest::test]
async fn changing_the_role_away_from_admin_revokes_owned_tokens(pool: PgPool) {
    let owner = admin(&pool, "role-change@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "UPDATE ... SET role (away from admin) must revoke the owner's tokens"
    );
}

#[dbtest::test]
async fn deleting_the_grant_row_revokes_owned_tokens(pool: PgPool) {
    let owner = admin(&pool, "grant-delete@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("DELETE FROM platform_role_grants WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "DELETE of the owner's only admin grant row must revoke the owner's tokens"
    );
}

#[dbtest::test]
async fn moving_the_grant_to_another_user_revokes_the_original_owners_tokens(pool: PgPool) {
    let owner = admin(&pool, "grant-move-from@test.invalid").await;
    // A plain user with no grant of its own, so the moved row does not
    // collide with the active_platform_grant uniqueness constraint.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'grant-move-to@test.invalid')")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET user_id=$2 WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "moving the owner's only admin grant row to another user must revoke the original owner's tokens"
    );
}

/// The insert guard requires the caller to hold the owner's exclusive
/// user-scope lock already. A missing lock returns SQLSTATE `OM003`,
/// which maps to `CreateAdminTokenError::OwnerLockNotHeld`.
#[dbtest::test]
async fn create_without_the_owners_scope_lock_held_is_refused_with_om003(pool: PgPool) {
    let owner = admin(&pool, "no-lock@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    // Deliberately not taking `governance::locks::exclusive` first.
    let result =
        create_admin_token(&mut tx, owner, "n", &scopes(&["platform:read"]), 30, None).await;
    let _ = tx.rollback().await;
    assert!(
        matches!(result, Err(CreateAdminTokenError::OwnerLockNotHeld)),
        "expected OwnerLockNotHeld, got {:?}",
        result.as_ref().err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

/// Companion to the above: with the lock correctly taken up front, in the
/// same transaction, creation succeeds exactly as before.
#[dbtest::test]
async fn create_with_the_owners_scope_lock_held_succeeds(pool: PgPool) {
    let owner = admin(&pool, "with-lock@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    let created = create_admin_token(&mut tx, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!revoked(&pool, created.id).await);
}

/// A caller-supplied future creation time cannot extend the token lifetime.
/// The creation guard stamps the database time before the expiry CHECK
/// compares it with the requested expiry.
#[dbtest::test]
async fn a_forged_future_created_at_cannot_make_a_token_practically_unexpiring(pool: PgPool) {
    let owner = admin(&pool, "lifetime-bypass@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    let result = sqlx::query(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,created_at,expires_at)
         VALUES(gen_random_uuid(),$1,'attack',decode(repeat('c0',32),'hex'),ARRAY['platform:read'],now()+interval '10 years',now()+interval '10 years'+interval '90 days')",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await;
    let _ = tx.rollback().await;
    assert!(
        result.is_err(),
        "a forged ten-year-future created_at must not survive to make the token practically unexpiring"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "the attack insert must not have left any row behind"
    );
}

/// Owner cleanup revokes the token and scrubs its recorded last-use address.
#[dbtest::test]
async fn cleanup_scrubs_the_last_used_address(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "cleanup-scrub@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    store
        .touch_admin_token_use(token.id, Some("203.0.113.77".parse().unwrap()))
        .await
        .unwrap();
    let before: Option<String> =
        sqlx::query_scalar("SELECT host(last_used_address) FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before.as_deref(), Some("203.0.113.77"));
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert!(
        crate::lifecycle::cleanup_user(&mut tx, owner)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let after: Option<String> =
        sqlx::query_scalar("SELECT host(last_used_address) FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, None, "cleanup must scrub the last-use address");
    assert!(
        revoked(&pool, token.id).await,
        "cleanup also revokes every token the owner holds"
    );
}

/// Authentication returns the token's canonical IP allowlist on the principal
/// so the caller can enforce it before dispatch.
#[dbtest::test]
async fn authenticate_returns_the_tokens_canonical_ip_allowlist(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "principal-allowlist@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:DB8::/32".to_string()];
    let restricted = create(
        &pool,
        owner,
        "restricted",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
    .await
    .unwrap();
    let found = store
        .authenticate_admin_token(&restricted.token)
        .await
        .unwrap()
        .expect("a live token authenticates");
    assert_eq!(
        found.ip_allowlist,
        Some(vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()])
    );
    let unrestricted = create(
        &pool,
        owner,
        "unrestricted",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    let found = store
        .authenticate_admin_token(&unrestricted.token)
        .await
        .unwrap()
        .expect("a live token authenticates");
    assert_eq!(found.ip_allowlist, None);
}
