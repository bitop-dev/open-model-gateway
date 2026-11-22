//! The browser-only model selection branch refuses a token actor even when
//! called directly. This keeps token actors out of a lock sequence that
//! takes the user lock before the workspace and lineage locks.
use super::*;
use crate::identity::AdminTokenContext;

#[dbtest::test]
async fn change_model_non_direct_branch_refuses_a_token_actor(pool: PgPool) {
    let f = fixture(&pool).await;
    let team = f.team;
    let model = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO models(id,public_name,supported_protocols,enabled) VALUES($1,'guard/model',ARRAY['chat_completions'],true)",
    )
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();

    let token_actor = ManagementPrincipal::AdminToken(AdminTokenContext {
        token_id: Uuid::new_v4(),
        owner_user_id: f.owner.user_id,
        scopes: vec!["catalog:write".to_string()],
        ip_allowlist: None,
        client_address: None,
    });
    let result = catalogs::change_model(f.s.clone(), token_actor, team, model, true, false).await;
    assert!(
        result.is_err_and(|e| e.0 == StatusCode::FORBIDDEN),
        "a token actor must be refused before this branch's later Workspace/Lineage \
         locks could ever run out of order against its earlier User-scope lock"
    );
    // No grant was made: the guard ran before any write.
    let granted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2)",
    )
    .bind(team)
    .bind(model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!granted);

    // A browser actor (the only one the real routes ever pass here) is
    // unaffected: still succeeds exactly as before this fix. `f.owner`
    // owns `f.team`, so it has `manage` authority there. Deselect (not a
    // catalog grant) needs no catalog-eligibility fixture.
    let browser = ManagementPrincipal::Browser(f.owner.clone());
    let _ = catalogs::change_model(f.s.clone(), browser, team, model, false, false)
        .await
        .unwrap();
}
