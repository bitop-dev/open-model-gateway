//! The OpenRouter video wire against a mock of its contract: what is sent,
//! how a job object is read (status, cost), content, and no delete.
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};

use super::super::{OpenRouterAdapter, OpenRouterConfig};
use super::*;
use crate::providers::secrets::{Secret, SecretResolver};

struct Key;
impl SecretResolver for Key {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "env:OPENROUTER_TEST_KEY");
        Secret::new("local-mock-openrouter-key".into())
    }
}

/// One request: method, path and query, authorization, JSON body.
type Seen = (String, String, Option<String>, Option<Value>);

#[derive(Default)]
struct Server {
    seen: Mutex<Vec<Seen>>,
    /// The job object `GET /videos/{id}` answers, as raw JSON text.
    job: Mutex<String>,
}

async fn serve(server: Arc<Server>) -> (String, tokio::task::JoinHandle<()>) {
    let seen =
        |s: &Server, method: &str, path: String, headers: &HeaderMap, body: Option<Value>| {
            s.seen.lock().unwrap().push((
                method.into(),
                path,
                headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_owned()),
                body,
            ));
        };
    let app = Router::new()
        .route(
            "/api/v1/videos",
            axum::routing::post(
                async move |State(s): State<Arc<Server>>, headers: HeaderMap, body: String| {
                    seen(&s, "POST", "/videos".into(), &headers, serde_json::from_str(&body).ok());
                    (
                        StatusCode::ACCEPTED,
                        [("content-type", "application/json")],
                        r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","polling_url":"/api/v1/videos/gen-vid-1-abcdefghijklmnopqrst","status":"pending","status_detail":"queued_at_provider"}"#,
                    )
                },
            ),
        )
        .route(
            "/api/v1/videos/{id}",
            axum::routing::get(
                async move |State(s): State<Arc<Server>>, Path(id): Path<String>, headers: HeaderMap| {
                    seen(&s, "GET", format!("/videos/{id}"), &headers, None);
                    (
                        [("content-type", "application/json")],
                        s.job.lock().unwrap().clone(),
                    )
                },
            ),
        )
        .route(
            "/api/v1/videos/{id}/content",
            axum::routing::get(
                async move |State(s): State<Arc<Server>>,
                            Path(id): Path<String>,
                            RawQuery(q): RawQuery,
                            headers: HeaderMap| {
                    seen(
                        &s,
                        "GET",
                        format!("/videos/{id}/content?{}", q.unwrap_or_default()),
                        &headers,
                        None,
                    );
                    ([("content-type", "video/mp4")], b"mp4 bytes".to_vec()).into_response()
                },
            ),
        )
        .with_state(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, task)
}

fn target() -> Deployment {
    Deployment {
        image_input: false,
        id: uuid::Uuid::new_v4(),
        provider: "openrouter".into(),
        upstream_model: "google/veo-3.1".into(),
        credential_ref: "env:OPENROUTER_TEST_KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![],
    }
}

fn request(size: Option<&str>) -> VideoRequest {
    VideoRequest {
        model: "company/clips".into(),
        prompt: "a calm lake".into(),
        seconds: Some(8),
        size: size.map(Into::into),
    }
}

#[tokio::test]
async fn the_openrouter_video_wire() {
    let server = Arc::new(Server::default());
    let (base, _task) = serve(server.clone()).await;
    let adapter = OpenRouterAdapter::for_test(Arc::new(Key), OpenRouterConfig::default(), base);
    let t = target();
    use crate::providers::ProviderAdapter;
    // The length always, the size only when named, the data-collection
    // preference, the server's key.
    for size in [None, Some("1280x720")] {
        let created = adapter.create_video(&t, request(size)).await.unwrap();
        assert_eq!(created.state, JobState::Queued);
        assert_eq!(created.cost, None, "only a completed job has a cost");
        let (method, path, auth, body) = server.seen.lock().unwrap().pop().unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/videos"));
        assert_eq!(auth.as_deref(), Some("Bearer local-mock-openrouter-key"));
        let mut expected = json!({"model":"google/veo-3.1","prompt":"a calm lake","duration":8,"provider":{"data_collection":"deny"}});
        if let Some(size) = size {
            expected["size"] = json!(size);
        }
        assert_eq!(body.unwrap(), expected);
    }
    let id = UpstreamId::parse("gen-vid-1-abcdefghijklmnopqrst").unwrap();
    // A completed job's cost, read from the number's text, exactly; any
    // doubt is unknown, never a zero.
    for (usage, cost) in [
        (
            r#"{"cost":0.288,"is_byok":false}"#,
            ReportedCost::Known(288_000),
        ),
        (r#"{"cost":1.0000001}"#, ReportedCost::Known(1_000_001)),
        (r#"{"cost":3e-1}"#, ReportedCost::Known(300_000)),
        (r#"{"cost":0}"#, ReportedCost::Known(0)),
        (r#"{"cost":null}"#, ReportedCost::Unknown),
        (r#"{}"#, ReportedCost::Unknown),
        (r#"{"cost":-0.5}"#, ReportedCost::Unknown),
        (r#"{"cost":"0.5"}"#, ReportedCost::Unknown),
        (r#"{"cost":1e30}"#, ReportedCost::Unknown),
        (r#"[0.000001]"#, ReportedCost::Unknown),
        (r#"[{"cost":0.000001}]"#, ReportedCost::Unknown),
        (r#"{"cost":{"value":1}}"#, ReportedCost::Unknown),
    ] {
        *server.job.lock().unwrap() = format!(
            r#"{{"id":"gen-vid-1-abcdefghijklmnopqrst","polling_url":"x","status":"completed","usage":{usage},"expires_at":1800000000}}"#
        );
        let v = adapter.retrieve_video(&t, &id).await.unwrap();
        assert_eq!(
            (v.state, v.cost),
            (JobState::Completed, Some(cost)),
            "{usage}"
        );
        assert_eq!(v.expires_at, Some(1_800_000_000));
    }
    // A number no JSON reader holds makes the object unreadable: no
    // observation at all (the job stays as it was), never a zero.
    *server.job.lock().unwrap() =
        r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","status":"completed","usage":{"cost":1e400}}"#
            .into();
    assert_eq!(
        adapter.retrieve_video(&t, &id).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
    *server.job.lock().unwrap() =
        r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","polling_url":"x","status":"completed"}"#.into();
    assert_eq!(
        adapter.retrieve_video(&t, &id).await.unwrap().cost,
        Some(ReportedCost::Unknown),
        "no usage at all"
    );
    // How a job ended is a fixed code; the provider's text is never kept.
    for (status, state, code) in [
        ("pending", JobState::Queued, None),
        ("in_progress", JobState::InProgress, None),
        ("failed", JobState::Failed, Some("upstream_failed")),
        ("cancelled", JobState::Failed, Some("upstream_cancelled")),
        ("expired", JobState::Failed, Some("upstream_expired")),
    ] {
        *server.job.lock().unwrap() = format!(
            r#"{{"id":"gen-vid-1-abcdefghijklmnopqrst","polling_url":"x","status":"{status}","progress":42.5,"error":"the prompt said: a calm lake"}}"#
        );
        let v = adapter.retrieve_video(&t, &id).await.unwrap();
        assert_eq!(
            (v.state, v.cost, v.error.as_ref().map(ErrorCode::as_str)),
            (state, None, code),
            "{status}"
        );
        assert_eq!(
            v.progress,
            Some(42),
            "a fractional percentage: its whole part"
        );
    }
    // Another id answered, an unknown status, or a member twice: invalid.
    for job in [
        r#"{"id":"gen-vid-2-abcdefghijklmnopqrst","status":"completed"}"#,
        r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","status":"done"}"#,
        r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","status":"completed","status":"failed"}"#,
    ] {
        *server.job.lock().unwrap() = job.into();
        assert_eq!(
            adapter.retrieve_video(&t, &id).await.err(),
            Some(InferenceError::InvalidUpstream),
            "{job}"
        );
    }
    // Content: the video only, at index 0.
    let content = adapter
        .video_content(&t, &id, VideoAsset::Video)
        .await
        .unwrap();
    assert_eq!(content.content_type, "video/mp4");
    let (_, path, auth, _) = server.seen.lock().unwrap().pop().unwrap();
    assert_eq!(
        path,
        "/videos/gen-vid-1-abcdefghijklmnopqrst/content?index=0"
    );
    assert_eq!(auth.as_deref(), Some("Bearer local-mock-openrouter-key"));
    let requests = server.seen.lock().unwrap().len();
    for asset in [VideoAsset::Thumbnail, VideoAsset::Spritesheet] {
        assert_eq!(
            adapter.video_content(&t, &id, asset).await.err(),
            Some(InferenceError::Unsupported)
        );
    }
    // No delete: nothing is sent, and the copy is not gone.
    assert_eq!(
        adapter.delete_video(&t, &id).await.unwrap(),
        crate::jobs::types::VideoDeletion::NotOffered
    );
    assert_eq!(server.seen.lock().unwrap().len(), requests, "nothing sent");
}

/// On the real path: a running gateway with the OpenRouter adapter against
/// the mock, the job poller and PostgreSQL. A created video is polled to
/// completion, charged OpenRouter's reported cost (marked as its figure),
/// its output stored and served by the gateway, and its delete answers that
/// OpenRouter's copy stays.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn an_openrouter_video_end_to_end(pool: sqlx::PgPool) {
    use crate::{
        billing::v3::Meter,
        governance::tests::db::fixture,
        inference::{Engine, EngineLimits, admission_validation_tests::serve as serve_app},
        jobs::{
            JobLimits, Jobs,
            db_tests::{bearer, line, na, test_files},
        },
        providers::ProviderRegistry,
    };
    let f = fixture(pool.clone()).await;
    let server = Arc::new(Server::default());
    let (base, _task) = serve(server.clone()).await;
    let (connection, model, route) = (
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'OpenRouter','openrouter','env:OPENROUTER_TEST_KEY',true)")
        .bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/veo',ARRAY['videos'])")
        .bind(model).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(f.principal.workspace_id)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds) VALUES($1,$2,$3,'google/veo-3.1',true,'{8}')")
        .bind(route).bind(model).bind(connection).execute(&pool).await.unwrap();
    // $0.40/s, one rate for every size: the hold is 8 s × $0.40.
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.push(line(Meter::OutputVideoSecondsMs, "400000", 1000));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
        .bind(uuid::Uuid::new_v4()).bind(route).bind(json!(lines)).execute(&pool).await.unwrap();
    sqlx::query("UPDATE installation_settings SET file_video_enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    let registry = || {
        let mut registry = ProviderRegistry::default();
        registry
            .register(Arc::new(OpenRouterAdapter::for_test(
                Arc::new(Key),
                OpenRouterConfig::default(),
                base.clone(),
            )))
            .unwrap();
        registry
    };
    let files = test_files().await;
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry(),
        EngineLimits::default(),
    )
    .unwrap();
    let (gateway, _gateway) = serve_app(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(files.clone())),
    )
    .await;
    let poller = Jobs::with(
        f.store.clone(),
        registry(),
        JobLimits {
            poll_interval: Some(std::time::Duration::from_secs(1)),
            ..JobLimits::default()
        },
    )
    .with_files(Some(files));
    let auth = bearer(&f, f.principal.workspace_id).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{gateway}/v1/videos"))
        .header("authorization", &auth)
        .json(&json!({"model":"company/veo","prompt":"a calm lake"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    assert_eq!(
        (created["status"].clone(), created["seconds"].clone()),
        (json!("queued"), json!("8")),
        "{created}"
    );
    let path = format!("{gateway}/v1/videos/{}", created["id"].as_str().unwrap());
    // OpenRouter finishes it and reports its cost (more than six decimals).
    *server.job.lock().unwrap() = r#"{"id":"gen-vid-1-abcdefghijklmnopqrst","status":"completed","usage":{"cost":2.9000001,"is_byok":false}}"#.into();
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    poller.poll_once().await.unwrap();
    let (state, held, actual, unbounded, evidence): (String, i64, Option<i64>, bool, Option<String>) = sqlx::query_as("SELECT r.state,r.held_microusd,r.actual_microusd,r.unbounded_cost,l.evidence FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id JOIN monetary_ledger l ON l.execution_id=r.execution_id AND l.kind='settlement'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (state.as_str(), held, actual, unbounded, evidence.as_deref()),
        (
            "settled",
            3_200_000,
            Some(2_900_001),
            false,
            Some("provider_reported_cost")
        )
    );
    // The output is stored once and served by the gateway.
    let read: Value = client
        .get(&path)
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["status"], "completed", "{read}");
    let response = client
        .get(format!("{path}/content"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(&response.bytes().await.unwrap()[..], b"mp4 bytes");
    let thumbnail = client
        .get(format!("{path}/content?variant=thumbnail"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(thumbnail.status(), StatusCode::BAD_REQUEST);
    // No delete at OpenRouter: the gateway's copy and record go, nothing is
    // sent, and the answer says OpenRouter's copy stays.
    let sent = server.seen.lock().unwrap().len();
    let deleted: Value = client
        .delete(&path)
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            deleted["deleted"].clone(),
            deleted["upstream_deleted"].clone()
        ),
        (json!(true), json!(false))
    );
    assert_eq!(server.seen.lock().unwrap().len(), sent);
}
