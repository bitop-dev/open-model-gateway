//! #52: a route's video options, set and cleared through PATCH by a Platform
//! Admin, validated, audited, outside the enablement revision and history,
//! and shown on the route.
use super::*;

async fn connection(pool: &PgPool) -> Uuid {
    let p = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Video','openai_compatible','none')").bind(p).execute(pool).await.unwrap();
    p
}

type Video = (Option<Vec<i32>>, Option<Vec<String>>, i64);
async fn video(pool: &PgPool, d: Uuid) -> Video {
    sqlx::query_as("SELECT video_seconds,video_sizes,revision FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn a_route_s_video_options_are_set_validated_audited_and_cleared(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "clips").await;
    let p = connection(&pool).await;
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        json!({"model_id":m,"provider_connection_id":p,"upstream_model":"clips","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let d = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(video(&pool, d).await, (None, None, 1), "none by default");
    let path = format!("/api/v1/platform/deployments/{d}");
    let options = json!({"seconds":[4,6,8],"sizes":["1280x720","720x1280"]});
    let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"video": options})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        video(&pool, d).await,
        (
            Some(vec![4, 6, 8]),
            Some(vec!["1280x720".into(), "720x1280".into()]),
            1
        ),
        "set, outside the enablement revision"
    );
    // Sizes are optional: without them a request may not name one.
    let lengths_only = json!({"seconds":[8]});
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"video": lengths_only}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(video(&pool, d).await, (Some(vec![8]), None, 1));
    // A repeated value changes and audits nothing.
    let (status, body) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"video": lengths_only}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Invalid values are refused with their reason; nothing changes.
    for bad in [
        json!({"seconds":[]}),
        json!({"seconds":[4,4]}),
        json!({"seconds":[0]}),
        json!({"seconds":[3601]}),
        json!({"seconds":[4],"sizes":[]}),
        json!({"seconds":[4],"sizes":["1280x720","1280x720"]}),
        json!({"seconds":[4],"sizes":["1280*720"]}),
        json!({"seconds":[4],"sizes":["01280x720"]}),
        json!({"seconds":[4],"sizes":["12800x720"]}),
        json!({"seconds":[4],"sizes":["0x1"]}),
        json!({"seconds":(1..=33).collect::<Vec<i32>>()}),
    ] {
        let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"video": bad})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"]["reason"], "video_options_invalid", "{bad}");
    }
    // A body that is not the options' shape is not read at all (422).
    for bad in [
        json!([[4]]),
        json!({"seconds":[4],"other":1}),
        json!({"sizes":["1280x720"]}),
        json!({"seconds":["4"]}),
    ] {
        let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"video": bad})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    assert_eq!(video(&pool, d).await, (Some(vec![8]), None, 1));
    // With the route's other fields, in one transaction: an invalid value
    // leaves the enablement as it was, and valid ones change both.
    let (status, body) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"enabled":false,"video":{"seconds":[0]}}),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (StatusCode::BAD_REQUEST, json!("video_options_invalid"))
    );
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(enabled, "nothing of a refused PATCH is applied");
    let (status, body) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"image_input":true,"video":{"seconds":[8]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let image: bool = sqlx::query_scalar("SELECT image_input FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(image);
    assert_eq!(
        video(&pool, d).await,
        (Some(vec![8]), None, 1),
        "the unchanged options are a no-op"
    );
    // Sizes need lengths, in the database too.
    assert!(
        sqlx::query("UPDATE deployments SET video_seconds=NULL,video_sizes='{1x1}' WHERE id=$1")
            .bind(d)
            .execute(&pool)
            .await
            .is_err()
    );
    // Platform Admins only.
    for u in [&f.auditor, &f.owner] {
        let (status, _) = call(&f.s, u, "PATCH", &path, json!({"video": null})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Shown on the route and in the list (an Auditor reads them).
    let (status, detail) = call(&f.s, &f.auditor, "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["video"], json!({"seconds":[8],"sizes":null}));
    let (_, list) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/deployments",
        Value::Null,
    )
    .await;
    let row = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == d.to_string())
        .unwrap()
        .clone();
    assert_eq!(row["video"], detail["video"]);
    // `null` clears them: the route then serves no video request.
    let (status, _) = call(&f.s, &f.admin, "PATCH", &path, json!({"video": null})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(video(&pool, d).await, (None, None, 1));
    let (_, detail) = call(&f.s, &f.admin, "GET", &path, Value::Null).await;
    assert_eq!(detail["video"], Value::Null);
    let audits: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE resource_id=$1 AND action='deployment.video_options_changed' ORDER BY created_at")
        .bind(d)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        audits,
        [
            json!({"video": options}),
            json!({"video": {"seconds":[8],"sizes":null}}),
            json!({"video": null}),
        ]
    );
}

/// The database checks a route's options exactly as the gateway does.
#[dbtest::test]
async fn the_database_checks_video_options_as_the_gateway_does(pool: PgPool) {
    let seconds: [&[i32]; 9] = [
        &[4],
        &[4, 8, 12],
        &[1, 3600],
        &[],
        &[0],
        &[3601],
        &[4, 4],
        &[-1],
        &[1; 33],
    ];
    for case in seconds {
        let rust = crate::management::deployment_video::VideoOptions {
            seconds: case.to_vec(),
            sizes: None,
        }
        .valid();
        let sql: bool = sqlx::query_scalar("SELECT video_seconds_valid($1)")
            .bind(case)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sql, rust, "{case:?}");
    }
    let sizes: [&[&str]; 12] = [
        &["1280x720"],
        &["1x1", "9999x9999"],
        &[],
        &["0x1"],
        &["01x1"],
        &["12345x1"],
        &["1280X720"],
        &["1280x720 "],
        &["1x1,2x2"],
        &["1280x720", "1280x720"],
        &["x720"],
        &["1280x"],
    ];
    for case in sizes {
        let rust = crate::management::deployment_video::VideoOptions {
            seconds: vec![4],
            sizes: Some(case.iter().map(|s| (*s).to_owned()).collect()),
        }
        .valid();
        let sql: bool = sqlx::query_scalar("SELECT video_sizes_valid($1)")
            .bind(case)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sql, rust, "{case:?}");
    }
    // Shapes only SQL can write: none of them is valid.
    for literal in [
        "'{NULL}'::int[]",
        "'{{4},{8}}'::int[]",
        "'[0:0]={4}'::int[]",
        "'[2:3]={4,8}'::int[]",
    ] {
        let valid: bool = sqlx::query_scalar(&format!("SELECT video_seconds_valid({literal})"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!valid, "{literal}");
    }
    for literal in [
        "'{NULL}'::text[]",
        "'{{1x1},{2x2}}'::text[]",
        "'[0:0]={1x1}'::text[]",
    ] {
        let valid: bool = sqlx::query_scalar(&format!("SELECT video_sizes_valid({literal})"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!valid, "{literal}");
    }
}

/// Embedded enterprise migrations through `last`.
fn migrator_through(last: i64) -> sqlx::migrate::Migrator {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= last).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

/// 0097 replaces exactly the 0016 video check: video jobs recorded before it
/// (a size always set) still satisfy the new checks, a video job may now
/// have no size, and a job of another kind still may not have one.
#[sqlx::test(migrations = false)]
async fn migration_0097_keeps_old_video_jobs_and_allows_a_job_without_a_size(pool: PgPool) {
    migrator_through(96).run(&pool).await.unwrap();
    let store = Store::new(pool.clone());
    crate::bootstrap::seed(&store, crate::config::Environment::Development)
        .await
        .unwrap();
    let (key, workspace): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,workspace_id FROM api_keys ORDER BY id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let job = |size: Option<&'static str>, kind: &'static str| {
        let pool = pool.clone();
        async move {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'company/smart','openai',false,'succeeded',$1,now(),now(),0,0)")
                .bind(id).bind(workspace).bind(key).bind(deployment).execute(&pool).await.unwrap();
            let (seconds, endpoint) = if kind == "video" {
                (Some(8), None)
            } else {
                (None, Some("/v1/chat/completions"))
            };
            sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,poll_deadline_at,video_seconds,video_size,batch_endpoint,completed_at,settled_at) VALUES($1,$2,$3,$4,$5,$1,'company/smart','openai',$6,'completed','completed',now()+interval '1 hour',$7,$8,$9,now(),now())")
                .bind(id).bind(kind).bind(workspace).bind(key).bind(deployment)
                .bind(format!("up_{}", id.simple())).bind(seconds).bind(size).bind(endpoint)
                .execute(&pool).await.map(|_| id)
        }
    };
    let old = [
        job(Some("720x1280"), "video").await.unwrap(),
        job(Some("1792x1024"), "video").await.unwrap(),
    ];
    assert!(job(None, "video").await.is_err(), "0016 needs a size");
    let checks = || async {
        let defs: Vec<String> = sqlx::query_scalar("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='public.async_jobs'::regclass AND contype='c' ORDER BY conname")
            .fetch_all(&pool)
            .await
            .unwrap();
        defs
    };
    let before = checks().await;
    migrator_through(97).run(&pool).await.unwrap();
    let after = checks().await;
    let removed: Vec<_> = before.iter().filter(|d| !after.contains(d)).collect();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(removed[0].contains("video_size IS NOT NULL"), "{removed:?}");
    // The old rows satisfy the new checks (NOT VALID adds them unchecked;
    // validating here proves it) and are unchanged.
    for c in [
        "async_jobs_video_seconds",
        "async_jobs_video_size",
        "async_jobs_output_failure",
    ] {
        sqlx::query(&format!("ALTER TABLE async_jobs VALIDATE CONSTRAINT {c}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let sizes: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT video_size FROM async_jobs WHERE id=ANY($1) ORDER BY video_size",
    )
    .bind(&old[..])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sizes, [Some("1792x1024".into()), Some("720x1280".into())]);
    job(None, "video")
        .await
        .expect("a video job without a size");
    assert!(
        job(Some("720x1280"), "batch").await.is_err(),
        "a size on a batch"
    );
    // A recorded output failure is final.
    let fail = |id: Uuid, code: &'static str| {
        sqlx::query("UPDATE async_jobs SET output_failure=$2 WHERE id=$1")
            .bind(id)
            .bind(code)
            .execute(&pool)
    };
    assert!(fail(old[1], "Not A Code").await.is_err(), "not a code");
    fail(old[0], "too_large").await.unwrap();
    assert!(
        fail(old[0], "download_failed").await.is_err(),
        "a failure changed"
    );
    assert!(
        sqlx::query("UPDATE async_jobs SET output_failure=NULL WHERE id=$1")
            .bind(old[0])
            .execute(&pool)
            .await
            .is_err(),
        "a failure cleared"
    );
}

/// A database at 0096 with jobs as the gateway writes them: two completed
/// videos (a length and a size), a completed batch. `stray` adds a batch
/// with a length alone, which 0016 allowed and no later check does. Then
/// 0097 as it was released. Returns the pool's ids: (videos, batch, the
/// stray batch, key, workspace, deployment).
async fn at_0097(
    pool: &PgPool,
    stray: bool,
) -> ([Uuid; 2], Uuid, Option<Uuid>, (Uuid, Uuid, Uuid)) {
    migrator_through(96).run(pool).await.unwrap();
    let store = Store::new(pool.clone());
    crate::bootstrap::seed(&store, crate::config::Environment::Development)
        .await
        .unwrap();
    let ids = scope(pool).await;
    let videos = [
        job(pool, ids, "video", "completed", Some(8), Some("720x1280"))
            .await
            .unwrap(),
        job(pool, ids, "video", "completed", Some(4), Some("1792x1024"))
            .await
            .unwrap(),
    ];
    let batch = job(pool, ids, "batch", "completed", None, None)
        .await
        .unwrap();
    let stray = if stray {
        Some(
            job(pool, ids, "batch", "completed", Some(8), None)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    migrator_through(97).run(pool).await.unwrap();
    (videos, batch, stray, ids)
}
/// The seeded key's (key, workspace) and a route.
async fn scope(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let (key, workspace): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,workspace_id FROM api_keys ORDER BY id LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments ORDER BY id LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    (key, workspace, deployment)
}
/// A job of `kind` in `state` with this length and size (and its execution).
async fn job(
    pool: &PgPool,
    (key, workspace, deployment): (Uuid, Uuid, Uuid),
    kind: &str,
    state: &str,
    seconds: Option<i32>,
    size: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'company/smart','openai',false,'succeeded',$1,now(),now(),0,0)")
        .bind(id).bind(workspace).bind(key).bind(deployment).execute(pool).await?;
    let endpoint = (kind == "batch").then_some("/v1/chat/completions");
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,poll_deadline_at,video_seconds,video_size,batch_endpoint,completed_at,settled_at) VALUES($1,$2,$3,$4,$5,$1,'company/smart','openai',$6,$7,$7,now()+interval '1 hour',$8,$9,$10,CASE WHEN $7='completed' THEN now() END,CASE WHEN $7='completed' THEN now() END)")
        .bind(id).bind(kind).bind(workspace).bind(key).bind(deployment)
        .bind(format!("up_{}", id.simple())).bind(state).bind(seconds).bind(size).bind(endpoint)
        .execute(pool).await?;
    Ok(id)
}
async fn set(pool: &PgPool, id: Uuid, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::query(&format!("UPDATE async_jobs SET {sql} WHERE id=$1"))
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}
async fn checks(pool: &PgPool) -> Vec<(String, String, bool)> {
    sqlx::query_as("SELECT conname::text,pg_get_constraintdef(oid),convalidated FROM pg_constraint WHERE conrelid='public.async_jobs'::regclass AND contype='c' ORDER BY conname")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// 0098 on a database at 0097 as released, holding only jobs the gateway
/// writes: every check is validated, and each one refuses what it is for (a
/// check replaced by `CHECK (true)` fails here); videos completed before it
/// await nothing; the output state follows its rules.
#[sqlx::test(migrations = false)]
async fn migration_0098_validates_the_video_checks_and_guards_the_output_state(pool: PgPool) {
    let (videos, batch, _, ids) = at_0097(&pool, false).await;
    migrator_through(98).run(&pool).await.unwrap();
    assert!(
        checks(&pool).await.iter().all(|(.., valid)| *valid),
        "every check is validated"
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs WHERE output_pending")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0, "videos completed before 0098 await nothing");
    // A video has a length and maybe a size; a batch has neither.
    job(&pool, ids, "video", "completed", Some(8), None)
        .await
        .expect("a video without a size");
    for (kind, seconds, size) in [
        ("video", None, None),
        ("video", None, Some("720x1280")),
        ("batch", Some(8), None),
        ("batch", None, Some("720x1280")),
    ] {
        assert!(
            job(&pool, ids, kind, "completed", seconds, size)
                .await
                .is_err(),
            "{kind} {seconds:?} {size:?}"
        );
    }
    let running = job(&pool, ids, "video", "in_progress", Some(4), None)
        .await
        .unwrap();
    // Awaiting an output: a completed, undeleted video without one.
    set(&pool, videos[1], "output_pending=true").await.unwrap();
    for (id, label) in [(running, "a running video"), (batch, "a batch")] {
        assert!(
            set(&pool, id, "output_pending=true").await.is_err(),
            "{label} awaits an output"
        );
        assert!(
            set(&pool, id, "output_failure='too_large'").await.is_err(),
            "{label} fails an output"
        );
    }
    assert!(
        set(&pool, videos[1], "deleted_at=now()").await.is_err(),
        "deleted while awaited"
    );
    assert!(
        set(&pool, videos[1], "output_failure='too_large'")
            .await
            .is_err(),
        "failed while awaited"
    );
    set(
        &pool,
        videos[1],
        "output_pending=false,output_failure='too_large'",
    )
    .await
    .unwrap();
    // A recorded failure is final, and excludes an output file.
    assert!(
        set(&pool, videos[0], "output_failure='Not A Code'")
            .await
            .is_err(),
        "not a code"
    );
    set(&pool, videos[0], "output_failure='too_large'")
        .await
        .unwrap();
    assert!(
        set(&pool, videos[0], "output_failure='download_failed'")
            .await
            .is_err(),
        "a failure changed"
    );
    assert!(
        set(&pool, videos[0], "output_failure=NULL").await.is_err(),
        "a failure cleared"
    );
    let (_, workspace, _) = ids;
    let file = Uuid::new_v4();
    sqlx::query("INSERT INTO stored_files(id,workspace_id,purpose,object_key,backend,encryption_key_id) VALUES($1,$2,'video_output','video_output/'||$2::text||'/'||$1::text,'memory','k1')")
        .bind(file)
        .bind(workspace)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        set(&pool, videos[0], &format!("output_file_id='{file}'"))
            .await
            .is_err(),
        "a file after a failure"
    );
    let fresh = job(&pool, ids, "video", "completed", Some(4), None)
        .await
        .unwrap();
    set(&pool, fresh, "output_pending=true").await.unwrap();
    assert!(
        set(&pool, fresh, &format!("output_file_id='{file}'"))
            .await
            .is_err(),
        "a file while still awaited"
    );
    set(
        &pool,
        fresh,
        &format!("output_pending=false,output_file_id='{file}'"),
    )
    .await
    .unwrap();
    // Never at insert.
    for (column, value) in [
        ("output_pending", "true"),
        ("output_failure", "'too_large'"),
    ] {
        let (key, workspace, deployment) = ids;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'company/smart','openai',false,'succeeded',$1,now(),now(),0,0)")
            .bind(id).bind(workspace).bind(key).bind(deployment).execute(&pool).await.unwrap();
        let error = sqlx::query(&format!("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,poll_deadline_at,video_seconds,completed_at,{column}) VALUES($1,'video',$2,$3,$4,$1,'company/smart','openai',$5,'completed',now(),4,now(),{value})"))
            .bind(id).bind(workspace).bind(key).bind(deployment).bind(format!("up_{}", id.simple()))
            .execute(&pool).await.expect_err(column).to_string();
        assert!(
            error.contains("recorded on an existing job only"),
            "{column}: {error}"
        );
    }
    // Route options: a non-1-based array written under 0097 is refused now.
    assert!(
        sqlx::query("UPDATE deployments SET video_seconds='[0:0]={4}' WHERE id=$1")
            .bind(ids.2)
            .execute(&pool)
            .await
            .is_err()
    );
}

/// A row the 0016 check allowed and no later one does (a batch with a
/// length) stops 0098, naming the count, with nothing of it applied; once
/// the operator has dealt with it, 0098 applies.
#[sqlx::test(migrations = false)]
async fn migration_0098_stops_on_a_job_the_0016_check_allowed(pool: PgPool) {
    let (_, _, stray, _) = at_0097(&pool, true).await;
    let before = checks(&pool).await;
    let error = migrator_through(98)
        .run(&pool)
        .await
        .expect_err("a batch with a length")
        .to_string();
    assert!(
        error.contains("1 non-video job(s) carry a video length or size"),
        "{error}"
    );
    assert_eq!(checks(&pool).await, before, "nothing applied");
    let column: Option<String> = sqlx::query_scalar("SELECT column_name::text FROM information_schema.columns WHERE table_name='async_jobs' AND column_name='output_pending'")
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert_eq!(column, None);
    // The length is immutable (0016 guard) and jobs are never deleted: the
    // operator sets the row aside.
    sqlx::raw_sql(&format!("ALTER TABLE async_jobs DISABLE TRIGGER async_jobs_no_delete; DELETE FROM async_jobs WHERE id='{}'; ALTER TABLE async_jobs ENABLE TRIGGER async_jobs_no_delete;", stray.unwrap()))
        .execute(&pool)
        .await
        .unwrap();
    migrator_through(98).run(&pool).await.unwrap();
    assert!(checks(&pool).await.iter().all(|(.., valid)| *valid));
}
