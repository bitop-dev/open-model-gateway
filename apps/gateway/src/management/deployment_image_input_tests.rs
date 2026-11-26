//! #24: a route's image-input opt-in, set at creation and through PATCH by
//! a Platform Admin, audited, outside the enablement revision and history.
use super::*;

async fn connection(pool: &PgPool) -> Uuid {
    let p = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Vision','openai','env:UNUSED')").bind(p).execute(pool).await.unwrap();
    p
}

async fn state(pool: &PgPool, d: Uuid) -> (bool, bool, i64, i64) {
    sqlx::query_as("SELECT enabled,image_input,revision,(SELECT count(*) FROM configuration_changes WHERE deployment_id=$1) FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn a_route_s_image_input_is_set_at_creation_and_by_patch(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "vision").await;
    let p = connection(&pool).await;
    let create = |image_input: Option<bool>| {
        let mut body = json!({"model_id":m,"provider_connection_id":p,"upstream_model":"vision","enabled":true});
        if let Some(v) = image_input {
            body["image_input"] = v.into();
        }
        body
    };
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        create(None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let off = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        state(&pool, off).await,
        (true, false, 1, 1),
        "off by default"
    );
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        create(Some(true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let on = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(state(&pool, on).await, (true, true, 1, 1));
    let created: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE resource_id=$1 AND action='deployment.image_input_changed'")
        .bind(on)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        created,
        [json!({"image_input":true})],
        "an opt-in at creation is audited"
    );

    let path = format!("/api/v1/platform/deployments/{off}");
    // The opt-in alone: no enablement revision or history.
    let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"image_input":true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&pool, off).await, (true, true, 1, 1));
    // Both at once: one enablement change, the opt-in kept.
    let (status, body) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"enabled":false,"image_input":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&pool, off).await, (false, false, 2, 2));
    // A repeated value changes and audits nothing.
    call(&f.s, &f.admin, "PATCH", &path, json!({"image_input":false})).await;
    let audits: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE resource_id=$1 AND action='deployment.image_input_changed' ORDER BY created_at")
        .bind(off)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        audits,
        [json!({"image_input":true}), json!({"image_input":false})]
    );
    // Neither field, or an unknown one: refused, nothing changed.
    for body in [
        json!({}),
        json!({"enabled":null,"image_input":null}),
        json!({"image_input":true,"other":1}),
    ] {
        let (status, _) = call(&f.s, &f.admin, "PATCH", &path, body.clone()).await;
        assert!(status.is_client_error(), "{body}");
    }
    assert_eq!(state(&pool, off).await, (false, false, 2, 2));
    // Platform Admins only.
    for u in [&f.auditor, &f.owner] {
        let (status, _) = call(&f.s, u, "PATCH", &path, json!({"image_input":true})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(!state(&pool, off).await.1);
    // Listed and shown.
    let (_, list) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/deployments",
        Value::Null,
    )
    .await;
    let rows = list["data"].as_array().unwrap();
    let row =
        |id: Uuid| rows.iter().find(|r| r["id"] == id.to_string()).unwrap()["image_input"].clone();
    assert_eq!((row(off), row(on)), (json!(false), json!(true)));
    let (status, detail) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/deployments/{on}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["image_input"], true);
}
