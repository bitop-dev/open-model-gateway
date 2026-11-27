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

/// Blocked lines never fail the clean ones: batch lines are exempt from the
/// per-key bound (their batch was accepted with a bounded line count), so 40
/// blocked lines throttle nothing.
#[dbtest::test]
async fn blocked_lines_never_fail_the_clean_ones(pool: PgPool) {
    let w = world_with(pool, false, false, 2, None).await;
    let f = &w.f;
    rule_set(f, "block").await;
    let hex: String = "0123456789abcdef".chars().cycle().take(96).collect();
    let secret = format!("{}_{}.{}", "omg", &hex[..32], &hex[32..]);
    let lines: Vec<Value> = (0..80)
        .map(|i| {
            if i % 2 == 0 {
                chat_line(
                    &format!("leak{i}"),
                    "company/smart",
                    &format!("use {secret}"),
                )
            } else {
                chat_line(&format!("clean{i}"), "company/smart", "hello")
            }
        })
        .collect();
    let file = w.upload(&f.principal, &lines).await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // Already at the key's bound: batch lines neither count nor are refused.
    for _ in 0..crate::guardrails::limits::RECORDED_PER_MINUTE {
        crate::guardrails::limits::RECORDED
            .note(f.principal.key_id, crate::guardrails::limits::now());
    }
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    let states: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT state,error_code,count(*) FROM batch_lines WHERE job_id=$1 GROUP BY 1,2 ORDER BY 1",
    )
    .bind(job.id)
    .fetch_all(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        states,
        [
            ("failed".into(), Some("guardrail_input_blocked".into()), 40),
            ("succeeded".into(), None, 40),
        ]
    );
}

/// A line that meets a full scan pool is tried again after the error's
/// delay and succeeds; it spends no attempt.
#[dbtest::test]
async fn a_line_that_meets_a_full_scan_pool_is_retried_and_succeeds(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let set = Uuid::now_v7();
    let mut tx = f.store.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    // A custom pattern: every scan takes a slot.
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,0,'Ids','custom','pattern','{custom}','block','user_text','off','fail_closed','{generation}','deny',ARRAY['ACME-[0-9]{4}'])")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let file = w
        .upload(
            &f.principal,
            &[chat_line("clean", "company/smart", "hello")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // The workspace's whole share is held until the line has waited for a
    // slot and given up (it entered the step and left it again).
    let slots = &crate::guardrails::limits::SHARED_SCANS;
    let ws = f.principal.workspace_id;
    let share = crate::guardrails::limits::SHARE;
    let mut held = Vec::new();
    for _ in 0..share {
        held.push(slots.acquire(ws).await.unwrap());
    }
    let release = tokio::spawn(async move {
        while slots.in_step(ws) == share {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        while slots.in_step(ws) > share {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        drop(held);
    });
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    release.await.unwrap();
    let job = w.reload(&job).await;
    let line: (String, i16) =
        sqlx::query_as("SELECT state,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        line,
        ("succeeded".into(), 1),
        "retried without spending an attempt"
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert!(
        out.iter().any(|l| l["response"]["status_code"] == 200),
        "{out:?}"
    );
}

/// A custom-pattern block rule of the principal's kind; its id.
async fn custom_rule(f: &crate::governance::tests::db::Fixture) -> Uuid {
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let (set, rule) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = f.store.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,0,'Ids','custom','pattern','{custom}','block','user_text','off','fail_closed','{generation}','deny',ARRAY['ACME-[0-9]{4}'])")
        .bind(rule).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rule
}

/// A line whose budget ran out on a try that is tried again records nothing
/// for that try: once a later try runs, its request shows no refusal.
#[dbtest::test]
async fn a_retried_spent_try_records_nothing_and_the_line_runs(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let rule = custom_rule(f).await;
    crate::guardrails::enforce::SPEND_BUDGET
        .lock()
        .unwrap()
        .insert(rule, [true].into());
    let file = w
        .upload(
            &f.principal,
            &[chat_line("clean", "company/smart", "hello")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    let line: (String, i16) =
        sqlx::query_as("SELECT state,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(line, ("succeeded".into(), 1));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_events")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "the spent try wrote nothing; the try that ran found nothing"
    );
    assert_eq!(
        crate::guardrails::enforce::SPEND_BUDGET.lock().unwrap()[&rule].len(),
        0,
        "the first try was spent"
    );
}

/// A line whose budget runs out on every try fails with
/// `guardrail_unavailable`, and only its last try's decisions are recorded.
#[dbtest::test]
async fn a_line_spent_on_every_try_records_its_last_try_only(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let rule = custom_rule(f).await;
    crate::guardrails::enforce::SPEND_BUDGET
        .lock()
        .unwrap()
        .insert(rule, [true; 10].into());
    let file = w
        .upload(
            &f.principal,
            &[chat_line("clean", "company/smart", "hello")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    let line: (String, Option<String>) =
        sqlx::query_as("SELECT state,error_code FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        line,
        ("failed".into(), Some("guardrail_unavailable".into()))
    );
    let rows: Vec<(String, bool)> = sqlx::query_as("SELECT action,refused FROM guardrail_events")
        .fetch_all(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, [("detector_failed".into(), true)], "one try recorded");
    assert_eq!(
        crate::guardrails::enforce::SPEND_BUDGET.lock().unwrap()[&rule].len(),
        7,
        "three tries in all"
    );
}

/// A line that never gets a scan slot during its free tries spends one of
/// its ordinary retries (`omg_retries`) before it fails, and succeeds once a
/// slot is free.
#[dbtest::test]
async fn a_line_without_a_slot_spends_its_ordinary_retries_after_its_free_tries(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    custom_rule(f).await;
    let file = w
        .upload(
            &f.principal,
            &[chat_line("clean", "company/smart", "hello")],
        )
        .await;
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_retries":"1"})),
        )
        .await
        .unwrap();
    // The workspace's share is held until the line's second attempt starts.
    let slots = &crate::guardrails::limits::SHARED_SCANS;
    let ws = f.principal.workspace_id;
    let mut held = Vec::new();
    for _ in 0..crate::guardrails::limits::SHARE {
        held.push(slots.acquire(ws).await.unwrap());
    }
    let pool = f.store.pool.clone();
    let job_id = job.id;
    let release = tokio::spawn(async move {
        loop {
            let attempts: Option<i16> =
                sqlx::query_scalar("SELECT attempts FROM batch_lines WHERE job_id=$1")
                    .bind(job_id)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            if attempts == Some(2) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        drop(held);
    });
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    release.await.unwrap();
    let job = w.reload(&job).await;
    let line: (String, i16) =
        sqlx::query_as("SELECT state,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        line,
        ("succeeded".into(), 2),
        "its free tries, then one ordinary retry"
    );
}

/// A line's last spent-budget try is recorded, so it ends the line with its
/// guardrail error even when the batch stops at that moment: Logs and the
/// line's result agree.
#[dbtest::test]
async fn a_recorded_try_ends_the_line_with_its_error_even_when_the_batch_stops(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let rule = custom_rule(f).await;
    crate::guardrails::enforce::SPEND_BUDGET
        .lock()
        .unwrap()
        .insert(rule, [true; 10].into());
    let file = w
        .upload(
            &f.principal,
            &[chat_line("clean", "company/smart", "hello")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // The batch is cancelled at the third try, the recorded one.
    crate::jobs::runner::tests_stop::cancel_at(job.id, 3);
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let line: (String, Option<String>) =
        sqlx::query_as("SELECT state,error_code FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        line,
        ("failed".into(), Some("guardrail_unavailable".into()))
    );
    let rows: Vec<(String, bool)> = sqlx::query_as("SELECT action,refused FROM guardrail_events")
        .fetch_all(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, [("detector_failed".into(), true)]);
    // The line is counted failed exactly once.
    assert_eq!(w.reload(&job).await.request_failed, Some(1));
}

/// A line whose first attempt ran and failed, and whose retry then waits to
/// be inspected when the batch stops, is reported with that attempt's error
/// and execution, never as not run.
#[dbtest::test]
async fn a_stopped_retry_of_a_line_that_ran_is_reported_with_its_attempt(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    let rule = custom_rule(f).await;
    // The first attempt is inspected; its retry's budget is spent.
    crate::guardrails::enforce::SPEND_BUDGET
        .lock()
        .unwrap()
        .insert(rule, [false, true, true].into());
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "fail")])
        .await;
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_retries":"1"})),
        )
        .await
        .unwrap();
    crate::jobs::runner::tests_stop::cancel_at(job.id, 1);
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    assert_eq!(w.openai.executed(), ["fail"], "the first attempt ran");
    let line: (String, Option<String>, i16) =
        sqlx::query_as("SELECT state,error_code,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        line,
        ("failed".into(), Some("upstream_unavailable".into()), 2)
    );
    let first: Uuid = sqlx::query_scalar(
        "SELECT e.id FROM inference_executions e JOIN async_jobs j ON e.batch_job_id=j.id WHERE j.id=$1",
    )
    .bind(job.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    let recorded: Uuid = sqlx::query_scalar("SELECT execution_id FROM batch_lines WHERE job_id=$1")
        .bind(job.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(recorded, first, "the line names the attempt that failed");
    let job = w.reload(&job).await;
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    let text = errors[0].to_string();
    assert!(!text.contains("batch_cancelled"), "{text}");
    assert!(
        text.contains(&first.simple().to_string()) || text.contains(&first.to_string()),
        "{text}"
    );
    // The line is counted failed exactly once.
    assert_eq!(w.reload(&job).await.request_failed, Some(1));
}

/// A line none of whose attempts reached a provider (no scan slot through its
/// free tries, then again on its ordinary retry) is reported as not run when
/// its batch stops while the retry waits: nothing was sent.
#[dbtest::test]
async fn a_stopped_retry_of_a_line_that_never_reached_a_provider_is_not_run(pool: PgPool) {
    let w = world_with(pool, false, false, 1, None).await;
    let f = &w.f;
    custom_rule(f).await;
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "hello")])
        .await;
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_retries":"1"})),
        )
        .await
        .unwrap();
    let slots = &crate::guardrails::limits::SHARED_SCANS;
    let mut held = Vec::new();
    for _ in 0..crate::guardrails::limits::SHARE {
        held.push(slots.acquire(f.principal.workspace_id).await.unwrap());
    }
    // Two free tries and the refusal that ends them, then the retry's first
    // try: the batch is cancelled there.
    crate::jobs::runner::tests_stop::cancel_at(job.id, 4);
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    drop(held);
    assert!(w.openai.executed().is_empty(), "nothing was sent");
    let line: (String, Option<String>, i16) =
        sqlx::query_as("SELECT state,error_code,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(line, ("failed".into(), Some("batch_cancelled".into()), 2));
    // The line is counted failed exactly once.
    assert_eq!(w.reload(&job).await.request_failed, Some(1));
}
