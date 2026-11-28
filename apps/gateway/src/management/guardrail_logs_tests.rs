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
    // For a personal workspace they see this month's totals only, and never
    // of its local layer (the refused request above was decided there).
    sqlx::query("INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count) SELECT gen_random_uuid(),gen_random_uuid(),workspace_id,api_key_id,'embeddings','company/smart','input',gen_random_uuid(),gen_random_uuid(),1,'type_default','pattern','secret','monitored',3 FROM guardrail_events WHERE root_request_id=$1")
        .bind(private)
        .execute(&pool)
        .await
        .unwrap();
    // One request's decision of a platform personal-data rule that found
    // two kinds, and another request's: platform staff see two decisions
    // per action and category, never which kinds were sent or how many.
    let personal_kind = "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,kind,action,count) SELECT gen_random_uuid(),$4,workspace_id,api_key_id,'embeddings','company/smart','input',$5,$5,1,'type_default','pattern','personal_data',$2,'monitored',$3 FROM guardrail_events WHERE root_request_id=$1";
    let (one, other, pd_rule) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (root, kind, count) in [(one, "us_ssn", 2), (one, "email", 5), (other, "email", 1)] {
        sqlx::query(personal_kind)
            .bind(private)
            .bind(kind)
            .bind(count)
            .bind(root)
            .bind(pd_rule)
            .execute(&pool)
            .await
            .unwrap();
    }
    // A platform rule a local tightening made stricter decided this one: its
    // outcome may be the owner's choice, so it is left out too.
    let tightened = "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused,tightened) SELECT gen_random_uuid(),gen_random_uuid(),workspace_id,api_key_id,'chat_completions','company/smart','input',gen_random_uuid(),gen_random_uuid(),1,'workspace_override','pattern','secret','blocked',1,true,true FROM guardrail_events WHERE root_request_id=$1";
    sqlx::query(tightened)
        .bind(private)
        .execute(&pool)
        .await
        .unwrap();
    let (status, totals) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/workspaces/{}/guardrails", f.personal),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{totals}");
    assert_eq!(
        totals["decisions_this_month"],
        json!([
            {"action":"monitored","category":"personal_data","kind":null,"refuses":null,"decisions":2,"findings":8},
            {"action":"monitored","category":"secret","kind":null,"refuses":null,"decisions":1,"findings":3}
        ])
    );
    assert!(!totals.to_string().contains("us_ssn"), "{totals}");
    // A shared workspace's totals count every layer and tightening, and
    // name the kind.
    sqlx::query(tightened)
        .bind(own)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(personal_kind)
        .bind(own)
        .bind("us_ssn")
        .bind(2)
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
    let (status, totals) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/workspaces/{}/guardrails", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{totals}");
    let mut seen: Vec<(String, String, i64)> = totals["decisions_this_month"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["action"].as_str().unwrap().to_owned(),
                t["category"].as_str().unwrap().to_owned(),
                t["decisions"].as_i64().unwrap(),
            )
        })
        .collect();
    seen.sort();
    assert!(
        seen.contains(&("blocked".into(), "secret".into(), 1)),
        "{seen:?}"
    );
    assert!(
        totals["decisions_this_month"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["category"] == "personal_data" && t["kind"] == "us_ssn"),
        "{totals}"
    );
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

/// #841: the monthly totals split only not readable (`skipped`) decisions,
/// by their rule's own verdict (`refuses`), whatever other decisions did to
/// the request; a rule's decision on one request counts once otherwise,
/// also when only one of its events is the refusing one.
#[dbtest::test]
async fn not_readable_totals_tell_refusing_rules_from_ones_that_let_through(pool: PgPool) {
    let f = fixture(&pool).await;
    let event = "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,kind,action,count,refused,refuses) VALUES(gen_random_uuid(),$1,$2,$3,'chat_completions','company/smart','input',$4,$4,1,$5,'pattern',$6,$7,$8,1,$9,$10)";
    // (workspace, key, request, rule, layer, category, kind, action, refused, refuses)
    type Row<'a> = (
        Uuid,
        Uuid,
        Uuid,
        Uuid,
        &'a str,
        &'a str,
        Option<&'a str>,
        &'a str,
        bool,
        bool,
    );
    let insert = async |row: Row<'_>| {
        let (ws, key, root, rule, layer, category, kind, action, refused, refuses) = row;
        sqlx::query(event)
            .bind(root)
            .bind(ws)
            .bind(key)
            .bind(rule)
            .bind(layer)
            .bind(category)
            .bind(kind)
            .bind(action)
            .bind(refused)
            .bind(refuses)
            .execute(&pool)
            .await
            .unwrap();
    };
    let totals = async |ws: Uuid| {
        let (status, v) = call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("/api/v1/platform/workspaces/{ws}/guardrails"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        v["decisions_this_month"].clone()
    };
    let k = key(&pool, f.team, f.owner.user_id).await;
    let (deny, allow, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (a, b, c, d) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    // A deny rule refusing what it can't read; one outranked by a finding
    // of another rule (still a refusing verdict); an allow rule letting it
    // through, alone and on a request another rule refused.
    insert((
        f.team,
        k,
        a,
        deny,
        "type_default",
        "secret",
        None,
        "skipped",
        true,
        true,
    ))
    .await;
    insert((
        f.team,
        k,
        b,
        deny,
        "type_default",
        "secret",
        None,
        "skipped",
        false,
        true,
    ))
    .await;
    insert((
        f.team,
        k,
        b,
        other,
        "type_default",
        "custom",
        None,
        "blocked",
        true,
        true,
    ))
    .await;
    insert((
        f.team,
        k,
        c,
        allow,
        "type_default",
        "secret",
        None,
        "skipped",
        false,
        false,
    ))
    .await;
    insert((
        f.team,
        k,
        d,
        allow,
        "type_default",
        "secret",
        None,
        "skipped",
        false,
        false,
    ))
    .await;
    insert((
        f.team,
        k,
        d,
        other,
        "type_default",
        "custom",
        None,
        "blocked",
        true,
        true,
    ))
    .await;
    assert_eq!(
        totals(f.team).await,
        json!([
            {"action":"blocked","category":"custom","kind":null,"refuses":null,"decisions":2,"findings":2},
            {"action":"skipped","category":"secret","kind":null,"refuses":false,"decisions":2,"findings":0},
            {"action":"skipped","category":"secret","kind":null,"refuses":true,"decisions":2,"findings":0}
        ])
    );
    // A personal workspace: a platform personal-data rule blocked one
    // request for two kinds (one event is the refusing one): platform staff
    // see one blocked decision, not one per kind. A platform deny rule whose
    // refusal the owner's own rule outranked still reads as refusing, so
    // nothing of the owner's rules shows.
    let pk = key(&pool, f.personal, f.owner.user_id).await;
    let (pd, e, g) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    insert((
        f.personal,
        pk,
        e,
        pd,
        "type_default",
        "personal_data",
        Some("email"),
        "blocked",
        true,
        true,
    ))
    .await;
    insert((
        f.personal,
        pk,
        e,
        pd,
        "type_default",
        "personal_data",
        Some("phone"),
        "blocked",
        false,
        true,
    ))
    .await;
    insert((
        f.personal,
        pk,
        g,
        deny,
        "type_default",
        "secret",
        None,
        "skipped",
        false,
        true,
    ))
    .await;
    insert((
        f.personal,
        pk,
        g,
        other,
        "workspace_local",
        "custom",
        None,
        "blocked",
        true,
        true,
    ))
    .await;
    assert_eq!(
        totals(f.personal).await,
        json!([
            {"action":"blocked","category":"personal_data","kind":null,"refuses":null,"decisions":1,"findings":2},
            {"action":"skipped","category":"secret","kind":null,"refuses":true,"decisions":1,"findings":0}
        ])
    );
}
