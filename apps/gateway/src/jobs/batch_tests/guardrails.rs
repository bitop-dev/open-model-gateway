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
