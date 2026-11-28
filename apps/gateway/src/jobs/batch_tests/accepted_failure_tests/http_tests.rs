//! Real loopback gateway + PostgreSQL, including a real adapter/mock upstream.
use super::super::http_support::{Resolver, Server, client};
use super::*;
use crate::providers::openrouter::{OpenRouterAdapter, OpenRouterConfig};

#[dbtest::test]
async fn running_gateway_openrouter_nonstream_200_rejection_keeps_free_zero(pool: PgPool) {
    let f = fixture(pool).await;
    price(&f, true).await;
    sqlx::query(
        "UPDATE provider_connections SET provider='openrouter',credential_ref='env:MOCK_ONLY'",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let upstream = Server::start(
        axum::Router::new()
            .fallback(|| async { axum::Json(json!({"error":{"code":400,"message":"rejected"}})) }),
    )
    .await;
    let adapter = OpenRouterAdapter::for_test(
        Arc::new(Resolver),
        OpenRouterConfig::default(),
        format!("{}/api/v1", upstream.base),
    );
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(adapter)).unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = Server::start(crate::http::router_with_engine(
        f.store.clone(),
        None,
        engine,
    ))
    .await;
    let bearer = crate::jobs::db_tests::bearer(&f, f.principal.workspace_id).await;
    let response = client().post(format!("{}/v1/chat/completions", gateway.base))
        .header("authorization", &bearer)
        .json(&json!({"model":"company/smart","messages":[{"role":"user","content":"test"}],"max_tokens":10,"stream":false}))
        .send().await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_rejected");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM inference_executions")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, id).await,
        ("settled".into(), Some(0), Some(0), false)
    );
    let tokens: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(tokens, (Some(0), Some(0)));
}

#[dbtest::test]
async fn running_gateway_accepted_video_failure_and_refusals(pool: PgPool) {
    use crate::jobs::db_tests::{self, Fake, upstream_video, video_setup};
    let f = fixture(pool).await;
    video_setup(&f).await;
    price(&f, true).await;
    let fake = Fake::default();
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    fake.push_video(upstream_video(JobState::Failed, None));
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake)).unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = Server::start(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(db_tests::test_files().await)),
    )
    .await;
    let auth = db_tests::bearer(&f, f.principal.workspace_id).await;
    let theirs = db_tests::bearer(&f, f.team.workspace_id).await;
    let client = client();
    let response = client
        .post(format!("{}/v1/videos", gateway.base))
        .header("authorization", &auth)
        .json(&json!({"model":"company/smart","prompt":"a calm lake","seconds":"8"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    let url = format!("{}/v1/videos/{id}", gateway.base);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .header("authorization", theirs)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for _ in 0..2 {
        let response = client
            .get(&url)
            .header("authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["status"], "failed");
    }
    let execution: Uuid = sqlx::query_scalar("SELECT execution_id FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    unknown(&f, execution, Usage::default(), Some(0)).await;
}

#[dbtest::test]
async fn running_gateway_native_batch_failure_and_refusals(pool: PgPool) {
    let mut w = world(pool, true, false, 1).await;
    price(&w.f, true).await;
    let runtime = FileStoreRuntime::memory();
    w.files = crate::filestore::FileStorage::new(w.f.store.clone(), runtime.clone());
    let mut registry = ProviderRegistry::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let capture = calls.clone();
    let upstream = Server::start(axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri, _: axum::body::Bytes| {
        capture.lock().unwrap().push((method.clone(), uri.path().to_owned()));
        async move {
            let body = match (method.as_str(), uri.path()) {
                ("POST", "/v1/files") => json!({"id":"file-in1","object":"file","bytes":1,"created_at":1,"filename":"batch.jsonl","purpose":"batch"}),
                ("POST", "/v1/batches") => json!({"id":"batch-up1","status":"validating","input_file_id":"file-in1"}),
                ("GET", "/v1/batches/batch-up1") => json!({"id":"batch-up1","status":"failed","input_file_id":"file-in1","request_counts":{"total":1,"completed":0,"failed":1}}),
                ("DELETE", "/v1/files/file-in1") => json!({"id":"file-in1","deleted":true}),
                _ => panic!("unexpected mock provider request"),
            };
            axum::Json(body)
        }
    })).await;
    let adapter = crate::providers::openai::OpenAiAdapter::for_test(
        Arc::new(Resolver),
        format!("{}/v1", upstream.base),
    );
    registry.register(Arc::new(adapter)).unwrap();
    w.jobs = Jobs::with(w.f.store.clone(), registry.clone(), JobLimits::default())
        .with_files(Some(runtime.clone()));
    let engine = Engine::new(
        Arc::new(w.f.store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap();
    let gateway = Server::start(
        crate::http::router_with_engine(w.f.store.clone(), None, engine)
            .layer(axum::Extension(runtime)),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, w.f.principal.workspace_id).await;
    let theirs = crate::jobs::db_tests::bearer(&w.f, w.f.team.workspace_id).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let client = client();
    let response = client.post(format!("{}/v1/batches", gateway.base)).header("authorization", &auth)
        .json(&json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    let url = format!("{}/v1/batches/{id}", gateway.base);
    assert_eq!(
        client
            .get(&url)
            .header("authorization", &theirs)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    w.poll().await;
    w.poll().await;
    for _ in 0..2 {
        let response = client
            .get(&url)
            .header("authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["status"], "failed");
    }
    let execution: Uuid = sqlx::query_scalar("SELECT execution_id FROM async_jobs")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    unknown(&w.f, execution, Usage::default(), Some(0)).await;
    // The refused foreign read never submits; terminal reads never poll again.
    let paths: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
    assert_eq!(
        paths,
        [
            "POST /v1/files",
            "POST /v1/batches",
            "GET /v1/batches/batch-up1",
            "DELETE /v1/files/file-in1"
        ]
    );
}

#[dbtest::test]
async fn embedding_batch_reports_residency_when_only_compatible_route_is_excluded(pool: PgPool) {
    embedding_batch_reports_blocked_compatible_route(pool, false).await;
}

#[dbtest::test]
async fn embedding_batch_reports_cooldown_when_only_compatible_route_is_cooling(pool: PgPool) {
    embedding_batch_reports_blocked_compatible_route(pool, true).await;
}

async fn embedding_batch_reports_blocked_compatible_route(pool: PgPool, cooling: bool) {
    let mut w = world(pool.clone(), false, false, 1).await;
    let f = &w.f;
    crate::governance::tests::db::embeddings_model(f).await;
    sqlx::query(
        "UPDATE provider_connections SET provider='openrouter',credential_ref='env:MOCK_ONLY'
         WHERE id=(SELECT provider_connection_id FROM deployments WHERE id=$1)",
    )
    .bind(f.deployment)
    .execute(&pool)
    .await
    .unwrap();
    let compatible = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled)
         SELECT $1,model_id,provider_connection_id,'nvidia/nemotron-3-embed-1b',true
         FROM deployments WHERE id=$2",
    )
    .bind(compatible)
    .bind(f.deployment)
    .execute(&pool)
    .await
    .unwrap();
    let price = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,
             output_microusd_per_million,input_token_limit,output_token_limit,pricing_version)
         VALUES($1,$2,1000000,0,100,0,1),($3,$4,3000000,0,100,0,1)",
    )
    .bind(Uuid::new_v4())
    .bind(f.deployment)
    .bind(price)
    .bind(compatible)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployment_routing(deployment_id,priority,residency)
         VALUES($1,0,'us'),($2,10,'eu')",
    )
    .bind(f.deployment)
    .bind(compatible)
    .execute(&pool)
    .await
    .unwrap();
    let (expected_status, expected_code) = if cooling {
        sqlx::query(
            "INSERT INTO deployment_health(deployment_id,open_until)
             VALUES($1,clock_timestamp()+interval '1 hour')",
        )
        .bind(compatible)
        .execute(&pool)
        .await
        .unwrap();
        (503, "model_temporarily_unavailable")
    } else {
        sqlx::query("INSERT INTO routing_policies(model_id,required_residency) VALUES($1,'us')")
            .bind(f.model)
            .execute(&pool)
            .await
            .unwrap();
        (404, "model_not_found")
    };

    let calls = Arc::new(Mutex::new(Vec::new()));
    let capture = calls.clone();
    let upstream = Server::start(axum::Router::new().fallback(
        move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| async move {
            capture.lock().unwrap().push((uri.path().to_owned(), body));
            axum::Json(json!({
                "object":"list","model":"nvidia/nemotron-3-embed-1b",
                "data":[{"object":"embedding","index":0,"embedding":vec![0.25; 2048]}],
                "usage":{"prompt_tokens":4,"total_tokens":4}
            }))
        },
    ))
    .await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenRouterAdapter::for_test(
            Arc::new(Resolver),
            OpenRouterConfig::default(),
            format!("{}/api/v1", upstream.base),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let runtime = FileStoreRuntime::memory();
    w.files = crate::filestore::FileStorage::new(f.store.clone(), runtime.clone());
    w.jobs = Jobs::new(f.store.clone(), &engine).with_files(Some(runtime.clone()));
    let gateway = Server::start(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(runtime)),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(f, f.principal.workspace_id).await;
    let client = client();
    let submit = |file: String| {
        client
            .post(format!("{}/v1/batches", gateway.base))
            .header("authorization", &auth)
            .json(&json!({"input_file_id":file,"endpoint":"/v1/embeddings","completion_window":"24h"}))
            .send()
    };
    let line = |id, dimensions| {
        json!({
            "custom_id":id,"method":"POST","url":"/v1/embeddings",
            "body":{"model":"company/smart","input":"fixture","dimensions":dimensions}
        })
    };
    let blocked = line("blocked", 2048);
    let unsupported = line("unsupported", 512);
    let before = admission_rowcounts(f).await;
    let file = w.upload(&f.principal, std::slice::from_ref(&blocked)).await;
    let response = submit(file).await.unwrap();
    let status = response.status();
    let retry = response.headers().get("retry-after").cloned();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, expected_status, "{body}");
    assert_eq!(body["error"]["code"], expected_code);
    if cooling {
        assert!(retry.unwrap().to_str().unwrap().parse::<u64>().unwrap() > 0);
    }
    assert_eq!(admission_rowcounts(f).await, before);

    for (lines, unsupported_line) in [
        (vec![unsupported.clone()], 1),
        (vec![blocked.clone(), unsupported.clone()], 2),
        (vec![unsupported, blocked.clone()], 1),
    ] {
        let response = submit(w.upload(&f.principal, &lines).await).await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["code"], "invalid_batch_input");
        let issues = body["errors"]["data"].as_array().unwrap();
        assert_eq!(issues.len(), 1, "{body}");
        assert_eq!(issues[0]["line"], unsupported_line);
        assert_eq!(issues[0]["code"], "unsupported_feature");
        assert_eq!(admission_rowcounts(f).await, before);
    }
    let runner = Runner::new(w.jobs.clone());
    assert!(!runner.run_next().await.unwrap());
    assert!(calls.lock().unwrap().is_empty());

    sqlx::query("DELETE FROM deployment_health WHERE deployment_id=$1")
        .bind(compatible)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM routing_policies WHERE model_id=$1")
        .bind(f.model)
        .execute(&pool)
        .await
        .unwrap();
    let response = submit(w.upload(&f.principal, &[blocked]).await)
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let id = crate::jobs::types::parse_client_id("batch_", body["id"].as_str().unwrap()).unwrap();
    let job = store::reload(&f.store, id).await.unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(job.deployment_id, compatible);
    assert_eq!(reservation(f, job.execution_id).await.1, Some(300));
    assert!(runner.run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let child: (Uuid, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT e.deployment_id,r.price_id,r.actual_microusd
         FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id
         WHERE e.batch_job_id=$1",
    )
    .bind(job.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(child, (compatible, price, Some(12)));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "/api/v1/embeddings");
    assert_eq!(calls[0].1["model"], "nvidia/nemotron-3-embed-1b");
    assert_eq!(calls[0].1["dimensions"], 2048);
}
