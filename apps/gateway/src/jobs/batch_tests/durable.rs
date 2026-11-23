//! A provider answer is charged once across either side of the result commit.
use super::*;
use super::{
    fence::{lapse, start},
    http_support::{Resolver, Server, client},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Loss {
    Crash,
    Takeover,
}
#[derive(Clone, Copy)]
enum Boundary {
    BeforeCommit,
    AfterCommit,
}

#[derive(Clone, Copy)]
enum Case {
    Fault(Boundary, Loss),
    Group,
    NestedTakeover,
    Quota,
    FailedCommit { retryable: bool },
    Retry { cancel: bool },
    Pressure,
    Measure(usize),
}

// A fresh worker process reopens the same encrypted files and disposable database.
async fn local_files(dir: &std::path::Path) -> FileStoreRuntime {
    use base64::Engine as _;
    let config = crate::filestore::FileStoreConfig::from_lookup(&|key| match key {
        "GATEWAY_FILE_STORE" => Some("local".into()),
        "GATEWAY_FILE_STORE_DIR" => Some(dir.join("files").to_str().unwrap().into()),
        "GATEWAY_FILE_ENCRYPTION_KEYS_ENV" => Some("TEST_KEY".into()),
        "TEST_KEY" => Some(format!(
            "test:{}",
            base64::engine::general_purpose::STANDARD
                .encode(std::array::from_fn::<u8, 32, _>(|i| i as u8))
        )),
        _ => None,
    })
    .unwrap();
    FileStoreRuntime::build(config).await.unwrap()
}

fn loopback_engine(store: &crate::store::Store, base: &str) -> Engine {
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{base}/v1"),
        )))
        .unwrap();
    Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap()
}

fn loopback_jobs(
    store: crate::store::Store,
    engine: Engine,
    files: FileStoreRuntime,
    concurrency: usize,
) -> Jobs {
    Jobs::with(
        store,
        engine.registry().clone(),
        JobLimits {
            batch_concurrency: concurrency,
            ..JobLimits::default()
        },
    )
    .with_engine(engine)
    .with_files(Some(files))
}

#[tokio::test]
#[ignore = "child process of the batch crash tests"]
async fn crash_worker() {
    let pool = runtime_pool(
        std::env::var("DATABASE_URL")
            .unwrap()
            .parse::<sqlx::postgres::PgConnectOptions>()
            .unwrap(),
        &std::env::var("BATCH_TEST_DB").unwrap(),
    )
    .await;
    let dir = std::env::var("BATCH_TEST_DIR").unwrap();
    let store = crate::store::Store::new(pool);
    let engine = loopback_engine(&store, &std::env::var("BATCH_TEST_UPSTREAM").unwrap());
    let jobs = loopback_jobs(store, engine, local_files(dir.as_ref()).await, 1);
    jobs.finalize_hold.arm();
    assert!(Runner::new(jobs).run_next().await.unwrap());
}

async fn runtime_pool(options: sqlx::postgres::PgConnectOptions, db: &str) -> PgPool {
    assert!(db.starts_with("_sqlx_test_"));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        })
        .connect_with(options.database(db))
        .await
        .unwrap();
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT current_user='gateway_runtime' AND NOT (rolsuper OR rolbypassrls) FROM pg_roles WHERE rolname=current_user"
    ).fetch_one(&pool).await.unwrap());
    pool
}

fn start_process(w: &World, dir: &std::path::Path, upstream: &Server) -> tokio::process::Child {
    let module = module_path!().split_once("::").unwrap().1;
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            &format!("{module}::crash_worker"),
        ])
        .env(
            "BATCH_TEST_DB",
            w.f.store.pool.connect_options().get_database().unwrap(),
        )
        .env("BATCH_TEST_DIR", dir)
        .env("BATCH_TEST_UPSTREAM", &upstream.base)
        // The parent shard's libtest output must contain just its own summary.
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn normal_upload(client: &reqwest::Client, base: &str, auth: &str) -> reqwest::StatusCode {
    let mut form = b"--race\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--race\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ordinary.txt\"\r\n\r\n".to_vec();
    form.resize(form.len() + 64 * 1024, b'x');
    form.extend_from_slice(b"\r\n--race--\r\n");
    client
        .post(format!("{base}/v1/files"))
        .header("authorization", auth)
        .header("content-type", "multipart/form-data; boundary=race")
        .body(form)
        .send()
        .await
        .unwrap()
        .status()
}

async fn hold_segment_metadata(
    pool: &PgPool,
    workspace: Uuid,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,storage_bytes) VALUES($1,1048576)")
        .bind(workspace).execute(pool).await.unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION pause_segment_commit() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN PERFORM pg_advisory_xact_lock(654715); RETURN NEW; END $$;
    CREATE TRIGGER pause_segment_commit BEFORE UPDATE OF committed_at ON stored_files
    FOR EACH ROW WHEN (OLD.filename LIKE 'batch_%_part_%' AND NEW.committed_at IS NOT NULL)
    EXECUTE FUNCTION pause_segment_commit()",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut hold = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(654715)")
        .execute(&mut *hold)
        .await
        .unwrap();
    hold
}

async fn run_case(pool: PgPool, case: Case) {
    let fault = match case {
        Case::Fault(boundary, loss) => Some((boundary, loss)),
        Case::NestedTakeover => Some((Boundary::AfterCommit, Loss::Takeover)),
        _ => None,
    };
    let size = match case {
        Case::Fault(..) | Case::FailedCommit { .. } => 3,
        Case::Group => 32,
        Case::NestedTakeover => 2,
        Case::Pressure => 24,
        Case::Quota | Case::Retry { .. } => 1,
        Case::Measure(size) => size,
    };
    let benchmark_uploads = matches!(case, Case::Measure(size) if size > 32);
    if matches!(case, Case::Measure(_)) {
        sqlx::raw_sql("CREATE SCHEMA bench_metrics; CREATE EXTENSION pg_stat_statements WITH SCHEMA bench_metrics")
            .execute(&pool)
            .await
            .unwrap();
    }
    let pressure = matches!(case, Case::Pressure);
    let concurrency = if pressure { 4 } else { 1 };
    let before = matches!(fault, Some((Boundary::BeforeCommit, _)));
    let mut w = world(pool.clone(), false, false, 1).await;
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,updated_by) VALUES($1,$2,$3)")
        .bind(w.f.deployment).bind(concurrency as i16).bind(w.f.owner).execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../../tests/common/ensure_runtime_role.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    w.f.store.apply_runtime_grants().await.unwrap();
    let options = pool.connect_options();
    let runtime = runtime_pool((*options).clone(), options.get_database().unwrap()).await;
    let store = crate::store::Store::new(runtime);

    if benchmark_uploads || matches!(case, Case::Quota) {
        sqlx::query(
            "UPDATE installation_settings SET file_user_files_enabled=true WHERE singleton",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    if benchmark_uploads {
        sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,storage_bytes) VALUES($1,33554432)")
            .bind(w.f.principal.workspace_id).execute(&pool).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let crash = matches!(fault, Some((_, Loss::Crash)));
    let files = local_files(dir.path()).await;
    w.files = crate::filestore::FileStorage::new(store.clone(), files.clone());
    let input: Vec<_> = (0..size)
        .map(|n| chat_line(&n.to_string(), "company/smart", "test"))
        .collect();
    let file = w.upload(&w.f.principal, &input).await;
    let calls = Arc::new(AtomicUsize::default());
    let seen = calls.clone();
    let fail_commit = matches!(case, Case::FailedCommit { .. });
    let retryable = matches!(
        case,
        Case::FailedCommit { retryable: true } | Case::Retry { .. }
    );
    let release_provider = Arc::new(tokio::sync::Semaphore::new(0));
    let release = release_provider.clone();
    let upstream = Server::start(axum::Router::new().fallback(move || {
        use axum::response::IntoResponse;
        let call = seen.fetch_add(1, Ordering::SeqCst);
        let release = release.clone();
        async move {
            if pressure && call > 0 {
                release.acquire().await.unwrap().forget();
            }
            if matches!(case, Case::Retry { .. }) && call == 0 {
                return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if fail_commit && call != 1 {
                let status = if retryable && call == 2 { axum::http::StatusCode::SERVICE_UNAVAILABLE } else { axum::http::StatusCode::BAD_REQUEST };
                return (status, axum::Json(json!({"error":{"message":"fixture rejected","type":"invalid_request_error"}}))).into_response();
            }
            axum::Json(json!({"id":"chatcmpl-test","model":"smart","choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_response()
        }
    })).await;
    let engine = loopback_engine(&store, &upstream.base);
    let gateway = Server::start(
        crate::http::router_with_engine(store.clone(), None, engine.clone())
            .layer(axum::Extension(w.files.runtime().clone())),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, w.f.principal.workspace_id).await;
    let client = client();
    let created = client.post(format!("{}/v1/batches", gateway.base)).header("authorization", &auth)
        .json(&json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h","metadata":{"omg_mode":"gateway","omg_retries": if retryable { "1" } else { "0" }}}))
        .send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    let id = created["id"].as_str().unwrap();
    let job = crate::jobs::store::reload(
        &w.f.store,
        Uuid::parse_str(id.strip_prefix("batch_").unwrap()).unwrap(),
    )
    .await
    .unwrap();
    w.jobs = loopback_jobs(store, engine, files, concurrency);
    if let Case::Retry { cancel } = case {
        check_backoff_retry(&w, &job, &client, &gateway.base, &auth, &calls, cancel).await;
        return;
    }
    if fail_commit {
        check_failed_commit(&w, &job, &client, &gateway.base, &auth, &calls).await;
        return;
    }
    let mut lock = w.f.store.pool.begin().await.unwrap();
    if before {
        sqlx::query("LOCK TABLE stored_files IN SHARE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
    } else if fault.is_some() && !crash {
        w.jobs.finalize_hold.arm();
    }
    let quota_hold = if matches!(case, Case::Quota | Case::Pressure) {
        Some(hold_segment_metadata(&pool, job.workspace_id).await)
    } else {
        None
    };
    let sum_calls = || {
        sqlx::query_scalar::<_,i64>(
        "SELECT coalesce(sum(calls),0)::bigint FROM bench_metrics.pg_stat_statements WHERE dbid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND query LIKE 'SELECT coalesce(sum(CASE WHEN f.committed_at%'"
    ).fetch_one(&pool)
    };
    let before_sums = if matches!(case, Case::Measure(_)) {
        sum_calls().await.unwrap()
    } else {
        0
    };
    let uploads = benchmark_uploads.then(|| {
        let (client, base, auth, calls) = (
            client.clone(),
            gateway.base.clone(),
            auth.clone(),
            calls.clone(),
        );
        tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(9), async {
                while calls.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let mut durations = Vec::new();
            for _ in 0..32 {
                assert!(
                    calls.load(Ordering::SeqCst) < size,
                    "upload started after the last provider call"
                );
                let at = std::time::Instant::now();
                assert_eq!(
                    normal_upload(&client, &base, &auth).await,
                    reqwest::StatusCode::OK
                );
                durations.push(at.elapsed());
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            durations.sort_unstable();
            eprintln!(
                "32 normal uploads, median {:?}, max {:?}",
                durations[16], durations[31]
            );
        })
    });
    let started = std::time::Instant::now();
    let mut process = crash.then(|| start_process(&w, dir.path(), &upstream));
    let old = (!crash).then(|| start(&w));
    if let Some((_, loss)) = fault {
        if before {
            tokio::time::timeout(Duration::from_secs(20),async {
                loop {
                    let waiting:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='stored_files'::regclass AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND NOT granted)").fetch_one(&w.f.store.pool).await.unwrap();
                    if waiting {break}
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                size,
                "the one-slot route drains before the first grouped write"
            );
        } else if crash {
            tokio::time::timeout(Duration::from_secs(20), async {
                while w.reload(&job).await.finalizing_at.is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            tokio::time::timeout(Duration::from_secs(20), w.jobs.finalize_hold.reached())
                .await
                .unwrap();
        }
        let rows: Vec<(String, Option<i32>, i32)> = sqlx::query_as("SELECT l.state,l.segment,j.request_completed FROM batch_lines l JOIN async_jobs j ON j.id=l.job_id WHERE j.id=$1").bind(job.id).fetch_all(&w.f.store.pool).await.unwrap();
        let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
            .await
            .unwrap();
        assert_eq!((cost.0, cost.1), (15 * size as i64, 0));
        if before {
            assert_eq!(rows, vec![("running".into(), None, 0); size]);
        } else {
            assert_eq!(rows, vec![("succeeded".into(), Some(0), size as i32); size]);
            let segment: Uuid = sqlx::query_scalar(
                "SELECT f.id FROM stored_files f JOIN batch_segments s ON s.file_id=f.id
                 WHERE s.job_id=$1 AND s.seq=0 AND f.committed_at IS NOT NULL
                   AND f.deleted_at IS NULL AND f.api_purpose IS NULL",
            )
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
            let segment = if matches!(case, Case::NestedTakeover) {
                use_old_segment_format(&w, &job, segment).await
            } else {
                segment
            };
            let segment = crate::filestore::files::public_id(segment);
            assert_eq!(
                client
                    .get(format!("{}/v1/files/{segment}/content", gateway.base))
                    .header("authorization", &auth)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND
            );
        }
        if let Loss::Crash = loss {
            let child = process.as_mut().unwrap();
            child.kill().await.unwrap();
            assert!(!child.wait().await.unwrap().success());
        }
        lapse(&w, &job).await;
        let owner = start(&w);
        tokio::time::timeout(Duration::from_secs(10), async {
            while sqlx::query_scalar::<_, i64>("SELECT runner_epoch FROM async_jobs WHERE id=$1")
                .bind(job.id)
                .fetch_one(&w.f.store.pool)
                .await
                .unwrap()
                == 1
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        lock.rollback().await.unwrap();
        w.jobs.finalize_hold.release();
        if let Some(old) = old {
            assert!(old.await.unwrap().unwrap());
        }
        assert!(owner.await.unwrap().unwrap());
    } else {
        lock.rollback().await.unwrap();
        if let Some(hold) = quota_hold {
            wait_for_metadata_commit(&pool, if pressure { 15 } else { 5 }).await;
            assert_eq!(w.reload(&job).await.request_completed, Some(0));
            if pressure {
                release_provider.add_permits(size);
                check_writer_backpressure(&pool, job.id, &calls).await;
            } else {
                assert_eq!(
                    normal_upload(&client, &gateway.base, &auth).await,
                    reqwest::StatusCode::OK,
                    "a normal 64 KiB upload fits while the exact-sized internal write is pending"
                );
            }
            hold.rollback().await.unwrap();
        }
        let limit = if matches!(case, Case::Measure(_)) {
            300
        } else {
            9
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(limit), old.unwrap())
                .await
                .expect("one-slot dispatch should drain")
                .unwrap()
                .unwrap()
        );
        let elapsed = started.elapsed();
        if let Some(uploads) = uploads {
            uploads.await.unwrap();
        }
        let seen_sums = if matches!(case, Case::Measure(_)) {
            sum_calls().await.unwrap() - before_sums
        } else {
            0
        };
        let (segments, lines, exact, deleted): (i64, i64, bool, bool) = sqlx::query_as(
            "SELECT count(*), coalesce(sum(s.lines),0), bool_and(f.reserved_bytes=f.size_bytes), bool_and(f.deleted_at IS NOT NULL)
             FROM batch_segments s JOIN stored_files f ON f.id=s.file_id WHERE s.job_id=$1"
        ).bind(job.id).fetch_one(&pool).await.unwrap();
        assert_eq!(lines, size as i64);
        assert!(deleted, "finalization removes the segment objects");
        if matches!(case, Case::Group | Case::Quota) {
            assert_eq!(segments, 1, "one bounded object for the settled responses");
            assert!(
                exact,
                "the pending byte reservation matches the encoded length"
            );
        }
        eprintln!(
            "{size} serial lines took {elapsed:?}; {segments} segment objects; {seen_sums} quota sums during run"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), size);
    let done = w.reload(&job).await;
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (15 * size as i64, 0));
    let failed = if before { size as i32 } else { 0 };
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(size as i32 - failed), Some(failed))
    );
    let status = client
        .get(format!("{}/v1/batches/{id}", gateway.base))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["status"], "completed");
    let file = status[if before {
        "error_file_id"
    } else {
        "output_file_id"
    }]
    .as_str()
    .unwrap();
    let content = client
        .get(format!("{}/v1/files/{file}/content", gateway.base))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let records: Vec<Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), size);
    let ids: std::collections::HashSet<_> = records
        .iter()
        .map(|r| r["custom_id"].as_str().unwrap())
        .collect();
    for n in 0..size {
        assert!(ids.contains(n.to_string().as_str()));
    }
    let other = crate::jobs::db_tests::bearer(&w.f, w.f.team.workspace_id).await;
    for path in [
        format!("/v1/batches/{id}"),
        format!("/v1/files/{file}/content"),
    ] {
        assert_eq!(
            client
                .get(format!("{}{path}", gateway.base))
                .header("authorization", &other)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    if before {
        assert!(
            records
                .iter()
                .all(|record| record["error"]["code"] == "result_unavailable")
        );
    } else {
        for record in records {
            if matches!(case, Case::NestedTakeover) && record["custom_id"] == "0" {
                assert_eq!(record["response"]["body"]["fixture"], nested_fixture());
            }
            assert_eq!(
                record["response"]["body"]["choices"][0]["message"]["content"],
                "answer"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn crash_before(pool: PgPool) {
    run_case(pool, Case::Fault(Boundary::BeforeCommit, Loss::Crash)).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_before(pool: PgPool) {
    run_case(pool, Case::Fault(Boundary::BeforeCommit, Loss::Takeover)).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn crash_after(pool: PgPool) {
    run_case(pool, Case::Fault(Boundary::AfterCommit, Loss::Crash)).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_after(pool: PgPool) {
    run_case(pool, Case::Fault(Boundary::AfterCommit, Loss::Takeover)).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn serial_results(pool: PgPool) {
    run_case(pool, Case::Group).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn pending_segment_leaves_room_for_an_ordinary_upload(pool: PgPool) {
    run_case(pool, Case::Quota).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "local timing fixture; PostgreSQL must preload pg_stat_statements"]
async fn measure_small(pool: PgPool) {
    run_case(pool, Case::Measure(32)).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "local timing fixture; PostgreSQL must preload pg_stat_statements"]
async fn measure_large(pool: PgPool) {
    run_case(pool, Case::Measure(2048)).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rolled_back_provider_failure_is_unavailable_after_takeover(pool: PgPool) {
    run_case(pool, Case::FailedCommit { retryable: false }).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rolled_back_retryable_failure_is_unavailable_without_retry(pool: PgPool) {
    run_case(pool, Case::FailedCommit { retryable: true }).await;
}

async fn check_failed_commit(
    w: &World,
    job: &JobRow,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    calls: &AtomicUsize,
) {
    let db = &w.f.store.pool;
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_failed_line() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'failed line commit refused' USING ERRCODE='23514'; END $$;
        CREATE CONSTRAINT TRIGGER refuse_failed_line AFTER UPDATE ON batch_lines
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
        WHEN (NEW.line_no=2 AND NEW.state='failed') EXECUTE FUNCTION refuse_failed_line()",
    )
    .execute(db)
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), start(w))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let attempts = || {
        sqlx::query_as::<_,(Uuid,String,Option<i64>)>(
        "SELECT e.id,r.state,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.batch_job_id=$1 ORDER BY e.id"
    ).bind(job.id).fetch_all(db)
    };
    let charged = attempts().await.unwrap();
    assert_eq!(charged.len(), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(w.reload(job).await.request_failed, Some(1));
    let lease: Option<gateway_datetime_compat::UtcInstant> =
        sqlx::query_scalar("SELECT runner_lease_until FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(db)
            .await
            .unwrap();
    assert!(lease.is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(10), start(w))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let final_job = w.reload(job).await;
    let file = public_id(final_job.error_file_id.unwrap());
    let content = client
        .get(format!("{base}/v1/files/{file}/content"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let errors: Vec<Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let error =
        |id: &str| errors.iter().find(|r| r["custom_id"] == id).unwrap()["error"]["code"].clone();
    assert_eq!(errors.len(), 2);
    assert_eq!(error("2"), "result_unavailable");
    assert_ne!(error("0"), "result_unavailable");
    assert_eq!(
        (final_job.request_completed, final_job.request_failed),
        (Some(1), Some(2))
    );
    let states: Vec<(String, Option<i32>)> =
        sqlx::query_as("SELECT state,segment FROM batch_lines WHERE job_id=$1 ORDER BY line_no")
            .bind(job.id)
            .fetch_all(db)
            .await
            .unwrap();
    assert_eq!(
        states,
        [
            ("failed".into(), Some(0)),
            ("succeeded".into(), Some(0)),
            ("interrupted".into(), None)
        ]
    );
    let segment_sizes: Vec<i32> =
        sqlx::query_scalar("SELECT lines FROM batch_segments WHERE job_id=$1 ORDER BY seq")
            .bind(job.id)
            .fetch_all(db)
            .await
            .unwrap();
    assert_eq!(segment_sizes, [2]);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(attempts().await.unwrap(), charged);
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn stalled_writer_drains_a_full_channel_with_multiple_producers(pool: PgPool) {
    run_case(pool, Case::Pressure).await;
}

async fn wait_for_metadata_commit(pool: &PgPool, seconds: u64) {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        while !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND objid=654715 AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))"
        ).fetch_one(pool).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

async fn check_writer_backpressure(pool: &PgPool, job: Uuid, calls: &AtomicUsize) {
    let awaiting = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM batch_lines WHERE job_id=$1 AND awaiting_segment",
        )
        .bind(job)
        .fetch_one(pool)
    };
    // One record in the stalled write, eight in the channel, and four
    // independent producers waiting for channel space.
    tokio::time::timeout(Duration::from_secs(8), async {
        while awaiting().await.unwrap() < 13 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(awaiting().await.unwrap(), 13);
    assert_eq!(calls.load(Ordering::SeqCst), 13);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_downloads_a_nested_result_in_an_existing_format_segment(pool: PgPool) {
    run_case(pool, Case::NestedTakeover).await;
}

fn nested_fixture() -> Value {
    (0..124).fold(
        json!("fixture with [braces], \"quotes\" and \\slashes"),
        |v, _| json!([v]),
    )
}

async fn use_old_segment_format(w: &World, job: &JobRow, original: Uuid) -> Uuid {
    // Keep real settled executions and progress. Replace only the encrypted
    // fixture object so it has the old n/e/r serialization and a deep result.
    let mut records = w.read(job.workspace_id, Some(original)).await;
    assert_eq!(records.len(), 2);
    let deep = &mut records.iter_mut().find(|r| r["n"] == 0).unwrap()["r"];
    deep["response"]["body"]["fixture"] = nested_fixture();
    let public = serde_json::to_vec(deep).unwrap();
    assert!(serde_json::from_slice::<Value>(&public).is_ok());
    let mut encoded = Vec::new();
    for record in records {
        serde_json::to_writer(&mut encoded, &record).unwrap();
        encoded.push(b'\n');
    }
    let file = w
        .files
        .create(
            crate::jobs::io::batch_file(
                true,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                "legacy-segment.jsonl".into(),
            ),
            Box::pin(futures_util::stream::once(async move {
                Ok(axum::body::Bytes::from(encoded))
            })),
        )
        .await
        .unwrap();
    let mut tx = w.f.store.pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE batch_segments DISABLE TRIGGER batch_segments_no_update")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query("UPDATE batch_segments SET file_id=$1 WHERE job_id=$2 AND file_id=$3")
            .bind(file.id)
            .bind(job.id)
            .bind(original)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    sqlx::query("ALTER TABLE batch_segments ENABLE TRIGGER batch_segments_no_update")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    w.files
        .delete(original, Some(job.workspace_id))
        .await
        .unwrap();
    file.id
}

#[dbtest::test]
async fn oversized_result_is_downloadable_as_a_public_error(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let p = w.f.principal;
    let input = w
        .upload(
            &p,
            &[chat_line("large", "company/smart", "oversized-result")],
        )
        .await;
    let job = w
        .create(p, &input, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert!(start(&w).await.unwrap().unwrap());
    let done = w.reload(&job).await;
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(0), Some(1))
    );
    assert!(done.output_file_id.is_none());
    let gateway = Server::start(
        crate::http::router_with_engine(w.f.store.clone(), None, w.jobs.engine.clone().unwrap())
            .layer(axum::Extension(w.files.runtime().clone())),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, p.workspace_id).await;
    let file = public_id(done.error_file_id.unwrap());
    let content = client()
        .get(format!("{}/v1/files/{file}/content", gateway.base))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let error: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(error["custom_id"], "large");
    assert_eq!(
        error["response"]["body"]["error"]["code"],
        "invalid_upstream_response"
    );
    assert_eq!(w.openai.executed(), ["oversized-result"]);
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (15, 0));
}

#[dbtest::test]
async fn backoff_retry_cancelled(pool: PgPool) {
    run_case(pool, Case::Retry { cancel: true }).await;
}

#[dbtest::test]
async fn backoff_retry_uncancelled(pool: PgPool) {
    run_case(pool, Case::Retry { cancel: false }).await;
}

async fn check_backoff_retry(
    w: &World,
    job: &JobRow,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    calls: &AtomicUsize,
    cancel: bool,
) {
    let pool = &w.f.store.pool;
    let runner = start(w);
    let first: Uuid = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(execution) = sqlx::query_scalar("SELECT execution_id FROM batch_lines WHERE job_id=$1 AND attempts=1 AND state='failed' AND deployment_id IS NOT NULL")
                .bind(job.id).fetch_optional(pool).await.unwrap() {
                break execution;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("first routed attempt has failed and released capacity");
    if cancel {
        client
            .post(format!(
                "{base}/v1/batches/{}/cancel",
                client_id("batch_", job.id)
            ))
            .header("authorization", auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(20), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let attempts = if cancel { 1 } else { 2 };
    assert_eq!(calls.load(Ordering::SeqCst), attempts);
    let reservations: Vec<Uuid> = sqlx::query_scalar("SELECT r.execution_id FROM governance_reservations r JOIN inference_executions e ON r.execution_id=e.id WHERE e.batch_job_id=$1 ORDER BY e.id")
        .bind(job.id).fetch_all(pool).await.unwrap();
    assert_eq!(reservations.len(), attempts);
    assert_eq!(reservations[0], first);
    let (execution, count): (Uuid, i16) =
        sqlx::query_as("SELECT execution_id,attempts FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(count as usize, attempts);
    assert_eq!(reservations.last(), Some(&execution));
    let done = w.reload(job).await;
    assert_eq!(done.request_total, Some(1));
    assert_eq!(done.request_completed, Some(i32::from(!cancel)));
    assert_eq!(done.request_failed, Some(i32::from(cancel)));
    assert_eq!(done.state, if cancel { "cancelled" } else { "completed" });
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!(cost.0, if cancel { 0 } else { 15 });
    let (file, other) = if cancel {
        (done.error_file_id, done.output_file_id)
    } else {
        (done.output_file_id, done.error_file_id)
    };
    assert!(other.is_none());
    let records = w.read(job.workspace_id, file).await;
    assert_eq!(records.len(), 1);
    let response = &records[0]["response"];
    assert_eq!(records[0]["custom_id"], "0");
    assert_eq!(response["request_id"], execution.to_string());
    if cancel {
        assert_eq!(response["body"]["error"]["code"], "upstream_unavailable");
    } else {
        assert_eq!(
            response["body"]["choices"][0]["message"]["content"],
            "answer"
        );
    }
}
