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
    call(
        &f.s,
        &f.admin,
        "PATCH",
        &path,
        json!({"video": lengths_only}),
    )
    .await;
    // Invalid options are refused with their reason; nothing changes.
    for bad in [
        json!({"seconds":[]}),
        json!({"seconds":[4,4]}),
        json!({"seconds":[0]}),
        json!({"seconds":[3601]}),
        json!({"seconds":[4],"sizes":[]}),
        json!([[4]]),
        json!({"seconds":[4],"sizes":["1280x720","1280x720"]}),
        json!({"seconds":[4],"sizes":["1280*720"]}),
        json!({"seconds":[4],"sizes":["01280x720"]}),
        json!({"seconds":[4],"sizes":["12800x720"]}),
        json!({"seconds":[4],"other":1}),
    ] {
        let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"video": bad})).await;
        assert!(status.is_client_error(), "{bad}: {body}");
        if status == StatusCode::BAD_REQUEST && body["error"]["reason"].is_string() {
            assert_eq!(body["error"]["reason"], "video_options_invalid", "{bad}");
        }
    }
    assert_eq!(video(&pool, d).await, (Some(vec![8]), None, 1));
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
