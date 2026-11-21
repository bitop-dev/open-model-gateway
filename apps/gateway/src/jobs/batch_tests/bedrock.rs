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
