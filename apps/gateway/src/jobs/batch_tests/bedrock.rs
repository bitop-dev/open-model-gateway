use super::*;
use crate::inference::admission_validation_tests::{Server, serve};
use crate::providers::bedrock::BedrockAdapter;

async fn bedrock_world(pool: PgPool) -> (World, Arc<Mutex<Vec<Value>>>, Server) {
    let mut world = world(pool.clone(), false, false, 1).await;
    sqlx::query(
        "UPDATE provider_connections SET provider='bedrock',credential_ref='aws:default',region='us-east-1'
         WHERE id=(SELECT provider_connection_id FROM deployments WHERE id=$1)",
    )
    .bind(world.claude_deployment)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE deployments SET upstream_model='meta.llama3-1-70b-instruct-v1:0' WHERE id=$1",
    )
    .bind(world.claude_deployment)
    .execute(&pool)
    .await
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let capture = calls.clone();
    let app = axum::Router::new().fallback(move |axum::Json(body): axum::Json<Value>| async move {
        capture.lock().unwrap().push(body);
        axum::Json(json!({
            "output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},
            "stopReason":"end_turn",
            "usage":{"inputTokens":4,"outputTokens":1,"totalTokens":5}
        }))
    });
    let (base, server) = serve(app).await;
    world
        .jobs
        .registry
        .register(Arc::new(BedrockAdapter::for_test(&base)))
        .unwrap();
    world.jobs.engine = Some(
        Engine::new(
            Arc::new(world.f.store.clone()),
            world.jobs.registry.clone(),
            EngineLimits::default(),
        )
        .unwrap(),
    );
    (world, calls, server)
}

fn failed_body() -> Value {
    json!({"model":"company/claude","max_tokens":10,"messages":[
        {"role":"user","content":"ping"},
        {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"ping","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"failed","is_error":true}]}],
        "tools":[{"name":"ping","input_schema":{"type":"object"}}]})
}

fn failed_line() -> Value {
    json!({"custom_id":"failed","method":"POST","url":"/v1/messages","body":failed_body()})
}

#[dbtest::test]
async fn failed_tool_result_batch_checks_price_and_ceiling_on_upload(pool: PgPool) {
    let (w, calls, _server) = bedrock_world(pool.clone()).await;
    let mut unpriced = failed_body();
    unpriced["model"] = json!("company/smart");
    // Use an unpriced deployment for the second alias.
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['messages'] WHERE id=$1")
        .bind(w.f.model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(w.f.deployment)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled)
         SELECT $1,$2,provider_connection_id,upstream_model,true FROM deployments WHERE id=$3",
    )
    .bind(Uuid::new_v4())
    .bind(w.f.model)
    .bind(w.claude_deployment)
    .execute(&pool)
    .await
    .unwrap();
    let mut over_ceiling = failed_body();
    over_ceiling["max_tokens"] = json!(51);
    let file = w.upload(&w.f.principal, &[
        json!({"custom_id":"over-ceiling","method":"POST","url":"/v1/messages","body":over_ceiling}),
        json!({"custom_id":"unpriced","method":"POST","url":"/v1/messages","body":unpriced}),
    ]).await;
    let Err(CreateError::Invalid(issues)) = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
    else {
        panic!("expected upload to report the missing price and exceeded ceiling");
    };
    let got: Vec<_> = issues
        .iter()
        .map(|issue| (issue.line, issue.error.code()))
        .collect();
    assert_eq!(got, [(1, "max_tokens_too_large"), (2, "model_not_priced")]);
    let rows: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM async_jobs),(SELECT count(*) FROM governance_reservations),
                (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM monetary_ledger)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, (0, 0, 0, 0));
    assert!(calls.lock().unwrap().is_empty());
}

#[dbtest::test]
async fn failed_tool_result_batch_preserves_upload_cooldown(pool: PgPool) {
    let (w, calls, _server) = bedrock_world(pool.clone()).await;
    sqlx::query("UPDATE deployments SET upstream_model='amazon.nova-pro-v1:0' WHERE id=$1")
        .bind(w.claude_deployment)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO deployment_health(deployment_id,open_until)
         VALUES($1,clock_timestamp()+interval '1 hour')",
    )
    .bind(w.claude_deployment)
    .execute(&pool)
    .await
    .unwrap();
    let file = w.upload(&w.f.principal, &[failed_line()]).await;
    let result = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await;
    assert!(matches!(
        result,
        Err(CreateError::Job(JobError::Inference(InferenceError::RouteCoolingDown(seconds))))
            if (3500..=3600).contains(&seconds)
    ));
    assert!(calls.lock().unwrap().is_empty());
}

#[dbtest::test]
async fn failed_tool_result_batch_sends_explicit_bedrock_status(pool: PgPool) {
    let (w, calls, _server) = bedrock_world(pool.clone()).await;
    sqlx::query(
        "UPDATE deployments SET upstream_model='new-region.anthropic.claude-sonnet-5-v1:0'
         WHERE id=$1",
    )
    .bind(w.claude_deployment)
    .execute(&pool)
    .await
    .unwrap();
    let file = w.upload(&w.f.principal, &[failed_line()]).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(job.deployment_id, w.claude_deployment);
    assert_eq!(reservation(&w.f, job.execution_id).await.1, Some(220));
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let capture = calls.lock().unwrap();
    assert_eq!(capture.len(), 1);
    assert_eq!(
        capture[0]["messages"][2]["content"][0]["toolResult"],
        json!({"toolUseId":"call","status":"error","content":[{"text":"failed"}]})
    );
}

#[dbtest::test]
async fn priority_batch_prices_and_dispatches_adaptive_thinking_on_anthropic(pool: PgPool) {
    adaptive_thinking_batch(pool, "priority", None).await;
}

#[dbtest::test]
async fn weighted_batch_prices_and_dispatches_adaptive_thinking_on_anthropic(pool: PgPool) {
    adaptive_thinking_batch(pool, "weighted", None).await;
}

enum BlockedRoute {
    Residency,
    Cooldown,
}

#[dbtest::test]
async fn priority_batch_refuses_residency_blocked_thinking(pool: PgPool) {
    adaptive_thinking_batch(pool, "priority", Some(BlockedRoute::Residency)).await;
}

#[dbtest::test]
async fn weighted_batch_refuses_residency_blocked_thinking(pool: PgPool) {
    adaptive_thinking_batch(pool, "weighted", Some(BlockedRoute::Residency)).await;
}

#[dbtest::test]
async fn priority_batch_refuses_cooling_thinking(pool: PgPool) {
    adaptive_thinking_batch(pool, "priority", Some(BlockedRoute::Cooldown)).await;
}

#[dbtest::test]
async fn weighted_batch_refuses_cooling_thinking(pool: PgPool) {
    adaptive_thinking_batch(pool, "weighted", Some(BlockedRoute::Cooldown)).await;
}

async fn adaptive_thinking_batch(pool: PgPool, strategy: &str, blocked: Option<BlockedRoute>) {
    use crate::providers::{
        anthropic::AnthropicAdapter,
        secrets::{Secret, SecretResolver},
    };

    struct Resolver;
    impl SecretResolver for Resolver {
        fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
            Secret::new("loopback-only".into())
        }
    }

    let (mut w, bedrock_calls, _bedrock) = bedrock_world(pool.clone()).await;
    sqlx::query("INSERT INTO routing_policies(model_id,strategy) VALUES($1,$2)")
        .bind(w.claude)
        .bind(strategy)
        .execute(&pool)
        .await
        .unwrap();
    let (connection, anthropic, price) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query(
        "INSERT INTO provider_connections(id,name,provider,credential_ref,enabled)
         VALUES($1,'Batch Anthropic','anthropic','env:LOOPBACK_ONLY',true)",
    )
    .bind(connection)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled)
         VALUES($1,$2,$3,'claude-loopback',false)",
    )
    .bind(anthropic)
    .bind(w.claude)
    .bind(connection)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployment_prices(
            id,deployment_id,input_microusd_per_million,output_microusd_per_million,
            input_token_limit,output_token_limit,pricing_version
         ) VALUES($1,$2,3000000,4000000,100,100,1)",
    )
    .bind(price)
    .bind(anthropic)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,0),($2,10)")
        .bind(w.claude_deployment)
        .bind(anthropic)
        .execute(&pool)
        .await
        .unwrap();

    let anthropic_calls = Arc::new(Mutex::new(Vec::new()));
    let capture = anthropic_calls.clone();
    let (upstream, _anthropic) = serve(axum::Router::new().fallback(
        move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| async move {
            capture.lock().unwrap().push((uri.path().to_owned(), body));
            axum::Json(json!({
                "id":"msg_loopback","type":"message","role":"assistant","model":"claude-loopback",
                "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
                "usage":{"input_tokens":4,"output_tokens":1,
                         "cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }))
        },
    ))
    .await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(w.jobs.registry.get("bedrock").unwrap())
        .unwrap();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{upstream}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(
        Arc::new(w.f.store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap();
    let runtime = FileStoreRuntime::memory();
    w.files = crate::filestore::FileStorage::new(w.f.store.clone(), runtime.clone());
    w.jobs = Jobs::new(w.f.store.clone(), &engine).with_files(Some(runtime.clone()));
    let (gateway, _gateway) = serve(
        crate::http::router_with_engine(w.f.store.clone(), None, engine)
            .layer(axum::Extension(runtime)),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, w.f.principal.workspace_id).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let create = |file: String, mode: Option<&str>| {
        let mut body =
            json!({"input_file_id":file,"endpoint":"/v1/messages","completion_window":"24h"});
        if let Some(mode) = mode {
            body["metadata"] = json!({"omg_mode": mode});
        }
        client
            .post(format!("{gateway}/v1/batches"))
            .header("authorization", &auth)
            .json(&body)
            .send()
    };
    let line = |max| {
        json!({
            "custom_id":"thinking","method":"POST","url":"/v1/messages",
            "body":{"model":"company/claude","max_tokens":max,"thinking":{"type":"adaptive"},
                    "messages":[{"role":"user","content":"ping"}]}
        })
    };
    let runner = Runner::new(w.jobs.clone());

    // At the Bedrock ceiling, the only refusal is the unsupported thinking option.
    let file = w.upload(&w.f.principal, &[line(50)]).await;
    let before = admission_rowcounts(&w.f).await;
    let response = create(file, None).await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_batch_input");
    assert_eq!(body["errors"]["data"][0]["line"], 1);
    assert_eq!(body["errors"]["data"][0]["code"], "invalid_body");
    assert_eq!(admission_rowcounts(&w.f).await, before);
    assert!(!runner.run_next().await.unwrap());
    assert!(bedrock_calls.lock().unwrap().is_empty());
    assert!(anthropic_calls.lock().unwrap().is_empty());

    // Bedrock has higher priority and a ceiling of 50. The compatible route
    // accepts 51, so its ceiling and price must govern admission and dispatch.
    sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
        .bind(anthropic)
        .execute(&pool)
        .await
        .unwrap();
    if let Some(blocked) = blocked {
        let (expected_status, code) = match blocked {
            BlockedRoute::Residency => {
                sqlx::query("UPDATE deployment_routing SET residency='us' WHERE deployment_id=$1")
                    .bind(w.claude_deployment)
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query(
                    "UPDATE routing_policies SET required_residency='us' WHERE model_id=$1",
                )
                .bind(w.claude)
                .execute(&pool)
                .await
                .unwrap();
                (404, "model_not_found")
            }
            BlockedRoute::Cooldown => {
                sqlx::query(
                    "INSERT INTO deployment_health(deployment_id,open_until)
                     VALUES($1,clock_timestamp()+interval '1 hour')",
                )
                .bind(anthropic)
                .execute(&pool)
                .await
                .unwrap();
                (503, "model_temporarily_unavailable")
            }
        };
        let file = w.upload(&w.f.principal, &[line(40)]).await;
        for mode in [None, Some("gateway")] {
            let response = create(file.clone(), mode).await.unwrap();
            let status = response.status();
            if matches!(blocked, BlockedRoute::Cooldown) {
                assert!(response.headers().get("retry-after").is_some(), "{status}");
            }
            let body: Value = response.json().await.unwrap();
            assert_eq!(status, expected_status, "{strategy} {mode:?}: {body}");
            assert_eq!(body["error"]["code"], code);
            assert_eq!(admission_rowcounts(&w.f).await, before);
        }
        assert!(!runner.run_next().await.unwrap());
        assert!(bedrock_calls.lock().unwrap().is_empty());
        assert!(anthropic_calls.lock().unwrap().is_empty());

        let mut compatible = line(40);
        compatible["body"]
            .as_object_mut()
            .unwrap()
            .remove("thinking");
        let file = w.upload(&w.f.principal, &[compatible]).await;
        let response = create(file, None).await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        let id =
            crate::jobs::types::parse_client_id("batch_", body["id"].as_str().unwrap()).unwrap();
        let job = store::reload(&w.f.store, id).await.unwrap();
        assert_eq!(job.mode(), Some(BatchMode::Gateway));
        assert_eq!(job.deployment_id, w.claude_deployment);
        assert_eq!(reservation(&w.f, job.execution_id).await.1, Some(280));
        assert!(runner.run_next().await.unwrap());
        let job = w.reload(&job).await;
        assert_eq!(
            (job.request_completed, job.request_failed),
            (Some(1), Some(0))
        );
        assert_eq!(bedrock_calls.lock().unwrap().len(), 1);
        assert!(anthropic_calls.lock().unwrap().is_empty());
        consistent(&w.f).await;
        return;
    }

    let file = w.upload(&w.f.principal, &[line(51)]).await;
    let response = create(file, None).await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let job = store::reload(
        &w.f.store,
        sqlx::query_scalar("SELECT id FROM async_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(job.deployment_id, anthropic);
    assert_eq!(reservation(&w.f, job.execution_id).await.1, Some(504));
    assert!(bedrock_calls.lock().unwrap().is_empty());
    assert!(anthropic_calls.lock().unwrap().is_empty());

    // A compatible route can enter cooldown after admission. The queued
    // line waits without a child attempt until its route recovers.
    sqlx::query(
        "INSERT INTO deployment_health(deployment_id,open_until)
         VALUES($1,clock_timestamp()+interval '1 hour')",
    )
    .bind(anthropic)
    .execute(&pool)
    .await
    .unwrap();
    let admitted = admission_rowcounts(&w.f).await;
    let mut run = std::pin::pin!(runner.run_next());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut run)
            .await
            .is_err()
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT runner_id IS NOT NULL FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(admission_rowcounts(&w.f).await, admitted);
    assert!(bedrock_calls.lock().unwrap().is_empty());
    assert!(anthropic_calls.lock().unwrap().is_empty());
    sqlx::query("DELETE FROM deployment_health WHERE deployment_id=$1")
        .bind(anthropic)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap()
    );
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let output = w.read(w.f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["response"]["status_code"], 200);
    let child: (Uuid, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT e.deployment_id, r.price_id, r.actual_microusd
         FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id
         WHERE e.batch_job_id=$1",
    )
    .bind(job.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(child, (anthropic, price, Some(16)));
    assert!(bedrock_calls.lock().unwrap().is_empty());
    {
        let calls = anthropic_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/v1/messages");
        assert_eq!(calls[0].1["model"], "claude-loopback");
        assert_eq!(calls[0].1["max_tokens"], 51);
        assert_eq!(calls[0].1["thinking"], json!({"type":"adaptive"}));
    }
    consistent(&w.f).await;
}
