//! Guardrail decisions in Logs (#840): a refused request (no attempt) is
//! listed as `blocked` under the same visibility as every request, with its
//! decisions in the detail; the guardrail filter; and attempt lists refuse it.
use super::*;

async fn key(pool: &PgPool, ws: Uuid, user: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'guardrail logs',$4,$1)")
        .bind(id)
        .bind(ws)
        .bind(user)
        .bind(vec![7u8; 32])
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn refused(pool: &PgPool, ws: Uuid, key: Uuid) -> Uuid {
    let root = Uuid::now_v7();
    sqlx::query("INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,streamed,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused) VALUES(gen_random_uuid(),$1,$2,$3,'messages','company/smart',true,'input',gen_random_uuid(),gen_random_uuid(),4,'workspace_local','pattern','custom','blocked',2,true)")
        .bind(root)
        .bind(ws)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    root
}
fn roots(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["root_request_id"].as_str().unwrap().to_owned())
        .collect()
}

#[dbtest::test]
async fn refused_requests_are_listed_as_blocked_with_logs_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    let own = refused(&pool, f.team, key(&pool, f.team, f.member.user_id).await).await;
    let other = refused(&pool, f.team, key(&pool, f.team, f.owner.user_id).await).await;
    let private = refused(
        &pool,
        f.personal,
        key(&pool, f.personal, f.owner.user_id).await,
    )
    .await;
    let list = format!("/api/v1/workspaces/{}/requests", f.team);
    // A member sees their own refused request; the owner sees both.
    let (status, mine) = call(&f.s, &f.member, "GET", &list, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(roots(&mine), [own.to_string()]);
    let row = &mine["data"][0];
    assert_eq!(
        (
            row["status"].clone(),
            row["attempts"].clone(),
            row["streamed"].clone(),
            row["cost_microusd"].clone()
        ),
        (json!("blocked"), json!(0), json!(true), json!("0"))
    );
    assert_eq!(
        row["guardrail"],
        json!({"refused":true,"actions":["blocked"],"categories":["custom"]})
    );
    let (_, all) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{list}?status=blocked&guardrail=refused"),
        Value::Null,
    )
    .await;
    assert_eq!(roots(&all).len(), 2);
    let (_, none) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{list}?guardrail=monitored"),
        Value::Null,
    )
    .await;
    assert!(roots(&none).is_empty());
    // The detail lists the decisions (ids, codes and counts) and no attempt.
    let (status, detail) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{list}/{other}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["attempts"], json!([]));
    let decision = &detail["guardrail_decisions"][0];
    assert_eq!(
        (
            decision["action"].clone(),
            decision["count"].clone(),
            decision["refused"].clone(),
            decision["revision"].clone()
        ),
        (json!("blocked"), json!(2), json!(true), json!(4))
    );
    let (status, _) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("{list}/{other}"),
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another member's request stays hidden"
    );
    // Platform staff see Team/Project requests, never a personal workspace's.
    let (_, staff) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/logs/requests?guardrail=refused",
        Value::Null,
    )
    .await;
    let staff = roots(&staff);
    assert!(staff.contains(&own.to_string()) && !staff.contains(&private.to_string()));
    // Attempt lists have nothing a refused request could match.
    for path in ["generations", "sessions"] {
        let (status, _) = call(
            &f.s,
            &f.owner,
            "GET",
            &format!("/api/v1/workspaces/{}/{path}?guardrail=any", f.team),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
}
