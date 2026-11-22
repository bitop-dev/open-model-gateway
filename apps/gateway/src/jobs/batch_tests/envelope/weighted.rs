use super::*;
use crate::inference::repository::InferenceRepository;
use sha2::{Digest, Sha256};

fn line_seed(batch: Uuid, line: u32) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(batch.as_bytes());
    hash.update(line.to_be_bytes());
    Uuid::from_bytes(hash.finalize()[..16].try_into().unwrap())
}

async fn weighted_world(pool: PgPool) -> (World, Uuid, Uuid) {
    let mut w = world(pool, false, false, 2).await;
    // The workspace ID is part of the sampler's seed.
    let workspace = Uuid::from_u128(702);
    let key = crate::auth::NewApiKey::generate().unwrap();
    let pool = &w.f.store.pool;
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Weighted Batch','team')")
        .bind(workspace)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')")
        .bind(workspace).bind(w.f.owner).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(workspace)
    .bind(w.claude)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'weighted',$4)")
        .bind(key.id).bind(workspace).bind(w.f.owner).bind(key.digest.as_slice()).execute(pool).await.unwrap();
    w.f.principal = Principal {
        workspace_id: workspace,
        key_id: key.id,
        user_id: Some(w.f.owner),
    };
    let expensive = second_claude_route(&w).await;
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,4000000,4000000,100,50,1)")
        .bind(Uuid::new_v4()).bind(expensive).execute(&w.f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO routing_policies(model_id,strategy) VALUES($1,'weighted')")
        .bind(w.claude)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_routing(deployment_id,weight) VALUES($1,1),($2,2)")
        .bind(w.claude_deployment)
        .bind(expensive)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let f = &w.f;
    let model = "company/claude";
    let candidates = f.store.deployments(&f.principal, model).await.unwrap();
    let candidates = candidates.as_slice();
    let first = |seed| async move {
        f.store
            .route_plan(&f.principal, model, candidates, seed)
            .await
            .unwrap()
            .deployment_ids[0]
    };
    // Find a reproducible witness for this workspace's routing hash: the
    // old parent uses the expensive route twice; the two line seeds split.
    for n in 1..=128 {
        let batch = Uuid::from_u128(n);
        if first(batch).await == expensive
            && first(line_seed(batch, 0)).await == w.claude_deployment
            && first(line_seed(batch, 1)).await == expensive
        {
            return (w, expensive, batch);
        }
    }
    panic!("no routing witness for the two weights");
}

async fn set_budget(w: &World, limit: i64) {
    crate::governance::set_test_budget(
        &w.f.store.pool,
        "local",
        None,
        Some(w.f.principal.workspace_id),
        None,
        "month",
        Some(limit),
    )
    .await;
}

async fn upload(w: &World) -> String {
    // A blank physical line must not shift the zero-based dispatch index.
    let first = chat_line("a", "company/claude", "one");
    let second = chat_line("b", "company/claude", "two");
    w.upload_raw(
        &w.f.principal,
        format!("{first}\n\n{second}\n").into_bytes(),
    )
    .await
}

async fn create(w: &World, id: Uuid, file: String) -> Result<JobRow, CreateError> {
    w.jobs
        .create_batch(
            w.f.principal,
            id,
            CreateBatch {
                input_file_id: file,
                endpoint: BatchEndpoint::ChatCompletions,
                metadata: None,
                completion_window_hours: None,
            },
        )
        .await
}

async fn children(w: &World, job: &JobRow) -> Vec<(Uuid, String, Option<i64>, Option<i64>)> {
    sqlx::query_as(
        "SELECT r.deployment_id, r.state, r.held_microusd, r.actual_microusd
         FROM batch_lines l JOIN governance_reservations r ON r.execution_id=l.execution_id
         WHERE l.job_id=$1 ORDER BY l.line_no",
    )
    .bind(job.id)
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap()
}

#[dbtest::test]
async fn weighted_lines_share_the_parent_estimate_and_transfer_their_own_holds(pool: PgPool) {
    let (w, expensive, id) = weighted_world(pool).await;
    let file = upload(&w).await;
    // Each line bounds 110 tokens. The split costs 660 µUSD; the old
    // batch-wide draw costs 880 µUSD.
    set_budget(&w, 650).await;
    assert!(matches!(
        create(&w, id, file.clone()).await,
        Err(CreateError::Job(JobError::Inference(
            InferenceError::BudgetExceeded(_)
        )))
    ));
    assert_eq!(admission_rowcounts(&w.f).await, (0, 0, 0, 0));

    set_budget(&w, 700).await;
    let job = create(&w, id, file)
        .await
        .expect("a 660 µUSD batch fits a 700 µUSD budget");
    let reads = &w.jobs.plan_lookup_counts;
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(
        (
            reads.deployments.load(Relaxed),
            reads.route_plan.load(Relaxed),
            reads.prices.load(Relaxed),
        ),
        (2, 2, 4),
        "each scan reads one candidate set and one routing snapshot, and reads each price once",
    );
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(reservation(&w.f, id).await.1, Some(660));
    assert_eq!(job.public_model, "company/claude");
    let work = w.read(w.f.principal.workspace_id, job.work_file_id).await;
    assert!(work.iter().all(|line| line.get("_omg_plan").is_none()));

    let first = w.anthropic.gate("one");
    let second = w.openai.gate("two");
    let runner = Runner::new(w.jobs.clone());
    let inspect = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(first.entered.notified(), second.entered.notified());
        })
        .await
        .expect("both estimated routes reached the upstream");
        assert_eq!(reservation(&w.f, id).await.1, Some(0));
        assert_eq!(
            children(&w, &job).await,
            [
                (w.claude_deployment, "pending".into(), Some(220), None),
                (expensive, "pending".into(), Some(440), None),
            ]
        );
        consistent(&w.f).await;
        first.release.notify_one();
        second.release.notify_one();
    };
    let (ran, ()) = tokio::join!(runner.run_next(), inspect);
    assert!(ran.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(0))
    );
    assert_eq!(reservation(&w.f, id).await.2, Some(0));
    assert_eq!(
        children(&w, &job).await,
        [
            (w.claude_deployment, "settled".into(), Some(220), Some(30)),
            (expensive, "settled".into(), Some(440), Some(60)),
        ]
    );
    consistent(&w.f).await;
}

#[dbtest::test]
async fn weighted_lines_recheck_route_cooldown_and_live_price_after_creation(pool: PgPool) {
    let (w, expensive, id) = weighted_world(pool).await;
    set_budget(&w, 700).await;
    let job = create(&w, id, upload(&w).await).await.unwrap();
    assert_eq!(reservation(&w.f, id).await.1, Some(660));

    sqlx::query("INSERT INTO deployment_health(deployment_id,open_until) VALUES($1,now()+interval '5 minutes')")
        .bind(w.claude_deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,8000000,8000000,100,50,1)")
        .bind(Uuid::new_v4()).bind(expensive).execute(&w.f.store.pool).await.unwrap();

    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "failed");
    assert_eq!(job.error_code.as_deref(), Some("budget_exceeded"));
    let route: Uuid =
        sqlx::query_scalar("SELECT deployment_id FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(route, expensive, "the estimated route entered cooldown");
    assert!(
        children(&w, &job).await.is_empty(),
        "the new 880 µUSD child exceeds the budget"
    );
    assert!(w.anthropic.executed().is_empty());
    assert!(w.openai.executed().is_empty());
    assert_eq!(reservation(&w.f, id).await.2, Some(0));
    consistent(&w.f).await;
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

#[dbtest::test]
async fn weighted_batch_through_running_gateway_and_loopback_openai(pool: PgPool) {
    use crate::providers::secrets::{Secret, SecretResolver};

    struct LoopbackSecret;
    impl SecretResolver for LoopbackSecret {
        fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
            Secret::new("mock-only-key".into())
        }
    }

    let (w, expensive, _) = weighted_world(pool).await;
    let f = &w.f;
    sqlx::query("UPDATE provider_connections SET provider='openai' WHERE provider='anthropic'")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let (upstream, upstream_task) = serve(axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            captured.lock().unwrap().push(body["model"].clone());
            async {
                axum::Json(json!({
                    "id":"chatcmpl-local","object":"chat.completion","created":1,
                    "model":"mock","choices":[{"index":0,"finish_reason":"stop",
                        "message":{"role":"assistant","content":"local answer"}}],
                    "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
                }))
            }
        }),
    ))
    .await;
    let runtime = FileStoreRuntime::memory();
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
            Arc::new(LoopbackSecret),
            format!("{upstream}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let jobs = Jobs::new(f.store.clone(), &engine).with_files(Some(runtime.clone()));
    let (gateway, gateway_task) = serve(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(runtime)),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(f, f.principal.workspace_id).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();
    let a = chat_line("a", "company/claude", "one");
    let b = chat_line("b", "company/claude", "two");
    let form = format!(
        "--m\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
         --m\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n\
         {a}\n\n{b}\n\r\n--m--\r\n"
    );
    let file: Value = client
        .post(format!("{gateway}/v1/files"))
        .header("authorization", &auth)
        .header("content-type", "multipart/form-data; boundary=m")
        .body(form)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let submit = || {
        client
            .post(format!("{gateway}/v1/batches"))
            .header("authorization", &auth)
            .json(
                &json!({"input_file_id":file["id"],"endpoint":"/v1/chat/completions",
                          "completion_window":"24h","metadata":{"omg_mode":"gateway"}}),
            )
            .send()
    };

    set_budget(&w, 430).await;
    let rejected: Value = submit().await.unwrap().json().await.unwrap();
    assert_eq!(rejected["error"]["code"], "budget_exceeded");
    assert_eq!(admission_rowcounts(f).await, (0, 0, 0, 0));
    assert!(calls.lock().unwrap().is_empty());

    set_budget(&w, 880).await;
    let batch: Value = submit()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = batch["id"].as_str().unwrap();
    let job = store::reload(&f.store, parse_client_id("batch_", id).unwrap())
        .await
        .unwrap();
    let candidates = f
        .store
        .deployments(&f.principal, "company/claude")
        .await
        .unwrap();
    let mut estimate = 0;
    let mut expected = Vec::new();
    for index in 0..2 {
        let plan = f
            .store
            .route_plan(
                &f.principal,
                "company/claude",
                &candidates,
                line_seed(job.execution_id, index),
            )
            .await
            .unwrap();
        let deployment = plan.deployment_ids[0];
        let (held, actual) = if deployment == expensive {
            (440, 60)
        } else {
            (220, 30)
        };
        estimate += held;
        expected.push((deployment, "settled".into(), Some(held), Some(actual)));
    }
    assert_eq!(reservation(f, job.execution_id).await.1, Some(estimate));
    assert!(Runner::new(jobs).run_next().await.unwrap());
    let completed: Value = client
        .get(format!("{gateway}/v1/batches/{id}"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["request_counts"]["completed"], 2);
    let results = client
        .get(format!(
            "{gateway}/v1/files/{}/content",
            completed["output_file_id"].as_str().unwrap()
        ))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut ids: Vec<Value> = results
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["custom_id"].clone())
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(ids, [json!("a"), json!("b")]);
    assert_eq!(children(&w, &job).await, expected);
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(reservation(f, job.execution_id).await.2, Some(0));
    consistent(f).await;
    gateway_task.abort();
    upstream_task.abort();
}
