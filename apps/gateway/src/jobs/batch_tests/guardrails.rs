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
