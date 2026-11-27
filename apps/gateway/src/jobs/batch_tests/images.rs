//! A Messages batch line with an image is planned only on a route the engine
//! would take at run time (`ProviderAdapter::route_takes`): a translated route
//! needs the deployment's `image_input` opt-in (0094), a route that forwards
//! the native envelope does not. A line no route takes is refused at
//! planning, before any hold.
use base64::Engine as _;

use super::*;

fn image_line(id: &str) -> Value {
    let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nbatch-image");
    json!({"custom_id":id,"method":"POST","url":"/v1/messages","body":{"model":"company/claude","max_tokens":10,"messages":[{"role":"user","content":[
        {"type":"text","text":"what is this?"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}]}]}})
}

async fn opt_in(w: &World) {
    sqlx::query("UPDATE deployments SET image_input=true WHERE id=$1")
        .bind(w.claude_deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// Runs the batch of one image line to completion on the gateway.
async fn runs(w: &World) {
    let file = w.upload(&w.f.principal, &[image_line("a")]).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    assert_eq!(w.anthropic.executed().len(), 1, "one call, on that route");
    assert!(w.openai.executed().is_empty());
}

#[dbtest::test]
async fn an_image_line_on_a_translated_route_needs_the_opt_in(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    {
        let mut script = w.anthropic.script.lock().unwrap();
        script.image_input = true;
        script.translates_messages = true;
    }
    let file = w.upload(&f.principal, &[image_line("a")]).await;
    let before = admission_rowcounts(f).await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::Messages, None)
        .await
    else {
        panic!("an image line no route takes is refused at planning");
    };
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.line, i.error.code()))
            .collect::<Vec<_>>(),
        [(1, "unsupported_feature")]
    );
    assert_eq!(admission_rowcounts(f).await, before, "nothing held");
    assert!(w.anthropic.executed().is_empty());
    assert!(w.anthropic.calls().is_empty());
    consistent(f).await;

    opt_in(&w).await;
    runs(&w).await;
    consistent(f).await;
}

#[dbtest::test]
async fn an_image_line_on_a_native_messages_route_needs_no_opt_in(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    w.anthropic.script.lock().unwrap().image_input = true;
    runs(&w).await;
    consistent(&w.f).await;
}

/// With two translated routes, the preferred one without the opt-in (and
/// without a price) and the other with it, the line is planned, priced, held
/// and run on the opted-in route alone: planning on the preferred route would
/// fail its price lookup, and a runner that claimed it would see the line
/// refused by the engine.
#[dbtest::test]
async fn an_image_line_skips_a_preferred_route_without_the_opt_in(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    {
        let mut script = w.anthropic.script.lock().unwrap();
        script.image_input = true;
        script.translates_messages = true;
    }
    let route = |image_input: bool| {
        let pool = f.store.pool.clone();
        let (model, template) = (w.claude, w.claude_deployment);
        async move {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,image_input) SELECT $1,$2,provider_connection_id,'claude-vision',true,$4 FROM deployments WHERE id=$3")
                .bind(id)
                .bind(model)
                .bind(template)
                .bind(image_input)
                .execute(&pool)
                .await
                .unwrap();
            id
        }
    };
    let (closed, open) = (route(false).await, route(true).await);
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,2000000,2000000,100,50,1)")
        .bind(Uuid::new_v4())
        .bind(open)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (deployment, priority) in [(closed, 0), (open, 10)] {
        sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,$2)")
            .bind(deployment)
            .bind(priority)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(w.claude_deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let file = w.upload(&f.principal, &[image_line("a")]).await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let executions: Vec<Uuid> =
        sqlx::query_scalar("SELECT deployment_id FROM inference_executions WHERE batch_job_id=$1")
            .bind(job.id)
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(executions, [open]);
    let closed_reservations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE deployment_id=$1")
            .bind(closed)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(closed_reservations, 0, "nothing held on the closed route");
    consistent(f).await;
}
