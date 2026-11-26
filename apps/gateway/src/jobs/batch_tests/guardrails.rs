//! A gateway-run batch line runs under its workspace's guardrails like an
//! interactive request (#840): a blocked line gets the interactive error body
//! in the error file and reaches no provider; the other lines run.
use super::*;

#[dbtest::test]
async fn a_blocked_line_gets_the_interactive_error_body(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let (set, rule) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = f.store.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}','block','user_text','off','fail_closed','{generation}','deny')")
        .bind(rule).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // A gateway key's shape, assembled here (never a real key).
    let hex: String = "0123456789abcdef".chars().cycle().take(96).collect();
    let secret = format!("{}_{}.{}", "omg", &hex[..32], &hex[32..]);
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("leaks", "company/smart", &format!("use {secret}")),
                chat_line("clean", "company/smart", "hello"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    let blocked: Vec<&Value> = errors
        .iter()
        .filter(|l| l["custom_id"] == "leaks")
        .collect();
    assert_eq!(blocked.len(), 1, "{errors:?}");
    let response = &blocked[0]["response"];
    assert_eq!(response["status_code"], 400);
    assert_eq!(response["body"]["error"]["code"], "guardrail_input_blocked");
    assert_eq!(
        response["body"]["error"]["guardrail"],
        json!({"policy_id": set, "rule_id": rule, "stage": "input", "category": "secret", "detector": "pattern"})
    );
    assert!(
        !errors.iter().any(|l| l.to_string().contains(&secret)),
        "the match is never echoed"
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert!(
        out.iter()
            .any(|l| l["custom_id"] == "clean" && l["response"]["status_code"] == 200),
        "{out:?}"
    );
    // The blocked line reserved nothing: only the clean line has an execution.
    let executions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM batch_lines b JOIN inference_executions e ON e.id=b.execution_id WHERE b.job_id=$1",
    )
    .bind(job.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(executions, 1);
    let recorded: (String, bool) = sqlx::query_as("SELECT action,refused FROM guardrail_events")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(recorded, ("blocked".into(), true));
}

/// A native batch's lines go to the provider's batch API, which this release
/// does not inspect: an enforcing rule refuses the batch before anything is
/// reserved (gateway-run mode inspects each line instead).
#[dbtest::test]
async fn a_native_batch_is_refused_by_an_enforcing_rule(pool: PgPool) {
    let w = world_with(pool, true, false, 1, None).await;
    let f = &w.f;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let mut tx = f.store.pool.begin().await.unwrap();
    let set = Uuid::now_v7();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}','block','user_text','off','fail_closed','{generation}','deny')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "hello")])
        .await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let live = "SELECT count(*) FROM stored_files WHERE deleted_at IS NULL";
    let files_before: i64 = sqlx::query_scalar(live)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let refused = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await;
    assert!(
        matches!(
            refused,
            Err(CreateError::Job(JobError::Inference(InferenceError::Guardrail(r))))
                if r.kind == crate::guardrails::enforce::RefusalKind::NotSupported
        ),
        "a native batch is refused"
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(before, after, "nothing was reserved");
    let files_after: i64 = sqlx::query_scalar(live)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        files_before, files_after,
        "the private copy of the input is gone"
    );
    let recorded: (String, bool) = sqlx::query_as("SELECT action,refused FROM guardrail_events")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(recorded, ("skipped".into(), true));
    // Gateway-run mode inspects the lines and runs the batch.
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode": "gateway"})),
        )
        .await;
    assert!(job.is_ok());
}

/// One type-default rule of the principal's workspace kind.
async fn rule_set(f: &crate::governance::tests::db::Fixture, action: &str) {
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let on_error = if action == "block" {
        "fail_closed"
    } else {
        "fail_open"
    };
    let set = Uuid::now_v7();
    let mut tx = f.store.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}',$3,'user_text','off',$4,'{generation}','deny')")
        .bind(Uuid::now_v7()).bind(set).bind(action).bind(on_error).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// A monitor rule does not enforce: a native batch runs natively, and its
/// unread lines are recorded `skipped`, not refused.
#[dbtest::test]
async fn a_native_batch_under_a_monitor_rule_is_created_and_recorded_skipped(pool: PgPool) {
    let w = world_with(pool, true, false, 1, None).await;
    let f = &w.f;
    rule_set(f, "monitor").await;
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "hello")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .expect("a monitor rule lets the batch through");
    let mode: String = sqlx::query_scalar("SELECT batch_mode FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(mode, "native");
    let recorded: Vec<(String, bool, String)> =
        sqlx::query_as("SELECT action,refused,operation FROM guardrail_events")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        recorded,
        [("skipped".into(), false, "chat_completions".into())]
    );
}

/// Blocked lines hold no route capacity: with a route that runs one line at
/// a time, the clean line after three blocked ones still runs in the same
/// pass, and nothing is left running or reserved.
#[dbtest::test]
async fn blocked_lines_release_the_route_capacity_they_were_scheduled_on(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,updated_by) VALUES($1,1,$2)")
        .bind(f.deployment).bind(f.owner).execute(&f.store.pool).await.unwrap();
    rule_set(f, "block").await;
    let hex: String = "0123456789abcdef".chars().cycle().take(96).collect();
    let secret = format!("{}_{}.{}", "omg", &hex[..32], &hex[32..]);
    let mut lines: Vec<Value> = (0..3)
        .map(|i| {
            chat_line(
                &format!("leak{i}"),
                "company/smart",
                &format!("use {secret}"),
            )
        })
        .collect();
    lines.push(chat_line("clean", "company/smart", "hello"));
    let file = w.upload(&f.principal, &lines).await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    let states: Vec<(i32, String)> =
        sqlx::query_as("SELECT line_no,state FROM batch_lines WHERE job_id=$1 ORDER BY line_no")
            .bind(job.id)
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        states,
        [
            (0, "failed".into()),
            (1, "failed".into()),
            (2, "failed".into()),
            (3, "succeeded".into()),
        ]
    );
    let held: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM batch_lines WHERE job_id=$1 AND state='running'),
                (SELECT count(*) FROM governance_reservations WHERE deployment_id=$2 AND state='pending')",
    )
    .bind(job.id)
    .bind(f.deployment)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(held, (0, 0), "no line or reservation holds the route");
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_events WHERE refused")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(refused, 3);
}
