//! Guardrail writes are authorized before their body is read: the route
//! policy layer (`authorization::require_route_access`) refuses a caller who
//! may not write the layer before the handler's JSON extractor runs, so a
//! refused caller never gets a custom pattern compiled. The handlers check
//! again in their own transaction, because that first check can be stale.
use super::*;

/// A full layer of patterns that compile within the limits.
fn expensive() -> Value {
    let patterns: Vec<String> = (0..50).map(|i| format!(r"(?i)[a-z]{{40}}{i}")).collect();
    json!({"local_only":false,"rules":[{"name":"Costly","protection":"custom","detector":{"kind":"pattern","categories":["custom"],"patterns":patterns},"input":{"action":"block"}}]})
}

async fn put(app: &Router, path: &str, s: &Session, body: String) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(path)
                .header("content-type", "application/json")
                .header("origin", ORIGIN)
                .header("cookie", format!("omg_session={}", s.cookie))
                .header("x-csrf-token", &s.csrf)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let captured = openapi::responses::capture("PUT", path, response).await;
    (
        captured.status,
        serde_json::from_slice(&captured.bytes).unwrap_or(Value::Null),
    )
}

#[dbtest::test]
async fn refused_guardrail_writers_never_reach_the_body(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (admin, auditor, owner, member, outsider) = (
        session(&pool, &f.admin).await,
        session(&pool, &f.auditor).await,
        session(&pool, &f.owner).await,
        session(&pool, &f.member).await,
        session(&pool, &f.outsider).await,
    );
    let type_team = "/api/v1/platform/workspace-types/team/guardrails".to_owned();
    let override_team = format!("/api/v1/platform/workspaces/{}/guardrails", f.team);
    let local_team = format!("/api/v1/workspaces/{}/guardrails", f.team);
    let refused = [
        (&type_team, &auditor, "auditor"),
        (&type_team, &owner, "workspace owner"),
        (&override_team, &auditor, "auditor"),
        (&override_team, &member, "member"),
        (&local_team, &member, "member"),
        (&local_team, &outsider, "non-member"),
        (&local_team, &auditor, "auditor"),
        (
            &local_team,
            &admin,
            "Platform Admin who is not a workspace admin",
        ),
    ];
    for (path, who, label) in refused {
        // Not even JSON: a refusal that read the body would answer 400.
        let (status, body) = put(&app, path, who, "{not json".to_owned()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} on {path}: {body}");
        let (status, body) = put(&app, path, who, expensive().to_string()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} on {path}: {body}");
    }
    // An authorized writer's body is read: the same malformed body is a 400.
    for (path, who) in [
        (&type_team, &admin),
        (&override_team, &admin),
        (&local_team, &owner),
    ] {
        let (status, body) = put(&app, path, who, "{not json".to_owned()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
    let mut local = expensive();
    local.as_object_mut().unwrap().remove("local_only");
    let (status, body) = put(&app, &local_team, &owner, local.to_string()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner's patterns compile: {body}"
    );
    assert_eq!(body["local"]["rules"][0]["detector"]["pattern_count"], 50);
}
