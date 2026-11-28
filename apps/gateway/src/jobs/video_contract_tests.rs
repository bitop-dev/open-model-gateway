//! #52 end to end: the OpenAI video contract on an approved
//! `openai_compatible` connection, against a mock contract server, through
//! the running gateway and the job poller on PostgreSQL. Per-route options
//! and refusals, the length always and the size only when named, settlement
//! from the seconds and size the server reports, the output stored in the
//! gateway's file store (and a download that fails), a server that never
//! finishes (poll budget), and a server without a delete (`405`).
use super::db_tests::{bearer, line, na, test_files};
use super::*;
use crate::{
    billing::v3::Meter,
    governance::tests::db::fixture,
    inference::{Engine, EngineLimits, admission_validation_tests::serve},
    providers::{ProviderRegistry, local::endpoints::ApprovedEndpoints, secrets::EnvSecrets},
};
use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub(super) const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42 mock video";
/// A mock video: its status, the seconds it was made with, the size it reports.
pub(super) type Video = (&'static str, String, Option<String>);

/// A content answer: status, content type, body, and a declared
/// `Content-Length` other than the body's.
pub(super) type Content = (u16, &'static str, Vec<u8>, Option<u64>);
/// A server of the OpenAI video contract: it remembers each create's form,
/// answers each video's status as set, serves (or fails) the content and
/// offers no delete (`405`) unless told otherwise.
#[derive(Default)]
pub(super) struct Contract {
    pub(super) forms: Mutex<Vec<String>>,
    /// Per video id: (status, seconds it was made with, size it reports).
    pub(super) videos: Mutex<std::collections::HashMap<String, Video>>,
    pub(super) content_fails: AtomicBool,
    /// Instead of the MP4: (status, content type, body, a declared
    /// `Content-Length` other than the body's).
    pub(super) content: Mutex<Option<Content>>,
    /// The delete's status (`405` when unset).
    pub(super) delete_status: Mutex<Option<u16>>,
    /// The id every create answers with (`vid_<n>` when unset).
    pub(super) fixed_id: Mutex<Option<String>>,
    /// The create's status instead of `200` (an error object then).
    pub(super) create_status: Mutex<Option<u16>>,
    pub(super) retrieves: AtomicUsize,
    pub(super) contents: AtomicUsize,
    pub(super) deletes: AtomicUsize,
}
pub(super) fn field(form: &str, name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\"\r\n\r\n");
    let start = form.find(&marker)? + marker.len();
    Some(form[start..].split("\r\n").next()?.to_owned())
}
fn object(id: &str, (status, seconds, size): &Video) -> Value {
    json!({"id":id,"object":"video","model":"clip-1","status":status,"progress":0,"seconds":seconds,"size":size})
}
pub(super) fn contract(state: Arc<Contract>) -> Router {
    Router::new()
        .route(
            "/v1/videos",
            axum::routing::post(
                async |State(c): State<Arc<Contract>>, body: axum::body::Bytes| {
                    let form = String::from_utf8_lossy(&body).into_owned();
                    if let Some(status) = *c.create_status.lock().unwrap() {
                        c.forms.lock().unwrap().push(form);
                        return (
                            StatusCode::from_u16(status).unwrap(),
                            axum::Json(json!({"error":{"message":"refused"}})),
                        )
                            .into_response();
                    }
                    let id =
                        c.fixed_id.lock().unwrap().clone().unwrap_or_else(|| {
                            format!("vid_{}", c.forms.lock().unwrap().len() + 1)
                        });
                    let seconds = field(&form, "seconds").unwrap_or_default();
                    // The server picks 1280x720 when no size is named.
                    let size = field(&form, "size").or_else(|| Some("1280x720".into()));
                    c.forms.lock().unwrap().push(form);
                    let video = ("queued", seconds, size);
                    let body = object(&id, &video);
                    c.videos.lock().unwrap().insert(id, video);
                    axum::Json(body).into_response()
                },
            ),
        )
        .route(
            "/v1/videos/{id}",
            axum::routing::get(
                async |State(c): State<Arc<Contract>>, Path(id): Path<String>| {
                    c.retrieves.fetch_add(1, Ordering::SeqCst);
                    let videos = c.videos.lock().unwrap();
                    match videos.get(&id) {
                        Some(video) => axum::Json(object(&id, video)).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            )
            .delete(
                async |State(c): State<Arc<Contract>>, Path(id): Path<String>| {
                    c.deletes.fetch_add(1, Ordering::SeqCst);
                    match *c.delete_status.lock().unwrap() {
                        None => StatusCode::METHOD_NOT_ALLOWED.into_response(),
                        Some(200) => {
                            axum::Json(json!({"id":id,"object":"video.deleted","deleted":true}))
                                .into_response()
                        }
                        Some(status) => StatusCode::from_u16(status).unwrap().into_response(),
                    }
                },
            ),
        )
        .route(
            "/v1/videos/{id}/content",
            axum::routing::get(async |State(c): State<Arc<Contract>>| {
                c.contents.fetch_add(1, Ordering::SeqCst);
                if c.content_fails.load(Ordering::SeqCst) {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                if let Some((status, media, body, declared)) = c.content.lock().unwrap().clone() {
                    // Streamed, so a declared length other than the body's is
                    // sent as declared; such a body then stalls instead of
                    // ending short (which would abort the response unsent).
                    let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(
                        bytes::Bytes::from(body),
                    )]);
                    let body = if declared.is_some() {
                        axum::body::Body::from_stream(futures_util::StreamExt::chain(
                            chunks,
                            futures_util::stream::pending(),
                        ))
                    } else {
                        axum::body::Body::from_stream(chunks)
                    };
                    let mut response = (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", media)],
                        body,
                    )
                        .into_response();
                    if let Some(n) = declared {
                        response
                            .headers_mut()
                            .insert("content-length", axum::http::HeaderValue::from(n));
                    }
                    return response;
                }
                ([("content-type", "video/mp4")], MP4).into_response()
            }),
        )
        .with_state(state)
}

pub(super) fn set_status(c: &Contract, id: &str, status: &'static str) {
    c.videos.lock().unwrap().get_mut(id).unwrap().0 = status;
}

#[dbtest::test]
async fn the_openai_video_contract_on_a_compatible_connection_end_to_end(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let mock = Arc::new(Contract::default());
    let (upstream, _upstream) = serve(contract(mock.clone())).await;
    let base = format!("{upstream}/v1");
    // A video model on a compatible connection: lengths 4 (default) and 8,
    // one size; a second model whose route has no video options.
    let (connection, clips, plain) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Clip server','openai_compatible','none',$2,true)")
        .bind(connection).bind(&base).execute(&pool).await.unwrap();
    for (model, name) in [(clips, "company/clips"), (plain, "company/plain")] {
        sqlx::query(
            "INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY['videos'])",
        )
        .bind(model)
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')").bind(f.principal.workspace_id).bind(model).execute(&pool).await.unwrap();
    }
    // $0.10/s by default, $0.20/s at 1280x720.
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.push(line(Meter::OutputVideoSecondsMs, "100000", 1000));
    let mut hd = line(Meter::OutputVideoSecondsMs, "200000", 1000);
    hd["variant"] = json!("1280x720");
    lines.push(hd);
    for (model, options) in [(clips, Some(("{4,8}", "{1280x720}"))), (plain, None)] {
        let route = Uuid::new_v4();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds,video_sizes) VALUES($1,$2,$3,'clip-1',true,$4::integer[],$5::text[])")
            .bind(route).bind(model).bind(connection)
            .bind(options.map(|o| o.0)).bind(options.map(|o| o.1))
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
            .bind(Uuid::new_v4()).bind(route).bind(json!(lines)).execute(&pool).await.unwrap();
    }
    // Three videos at once.
    sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('personal',10) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE installation_settings SET file_video_enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    let registry = || {
        let mut registry = ProviderRegistry::default();
        for adapter in crate::providers::local::adapters(
            Arc::new(EnvSecrets::new([])),
            ApprovedEndpoints::for_test(&base),
        ) {
            registry.register(adapter).unwrap();
        }
        registry
    };
    let files = test_files().await;
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry(),
        EngineLimits::default(),
    )
    .unwrap();
    let (gateway, _gateway) = serve(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(files.clone())),
    )
    .await;
    let poller = Jobs::with(
        f.store.clone(),
        registry(),
        JobLimits {
            poll_interval: Some(std::time::Duration::from_secs(1)),
            ..JobLimits::default()
        },
    )
    .with_files(Some(files));
    let auth = bearer(&f, f.principal.workspace_id).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let create = async |body: Value| {
        let response = client
            .post(format!("{gateway}/v1/videos"))
            .header("authorization", &auth)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json::<Value>().await.unwrap())
    };
    let get = async |path: String| {
        client
            .get(format!("{gateway}{path}"))
            .header("authorization", &auth)
            .send()
            .await
            .unwrap()
    };

    // Refusals before admission: nothing is sent, no job, no hold.
    for (body, code, param) in [
        (
            json!({"model":"company/clips","prompt":"a lake","seconds":"6"}),
            "unsupported_video_parameter",
            json!("seconds"),
        ),
        (
            json!({"model":"company/clips","prompt":"a lake","size":"640x480"}),
            "unsupported_video_parameter",
            json!("size"),
        ),
        (
            json!({"model":"company/plain","prompt":"a lake"}),
            "unsupported_capability",
            Value::Null,
        ),
    ] {
        let (status, error) = create(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {error}");
        assert_eq!(error["error"]["code"], code, "{body}");
        assert_eq!(error["error"]["param"], param, "{body}");
    }
    assert!(
        mock.forms.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);

    // The route's first length when none is named, and no size sent.
    let (status, a) = create(json!({"model":"company/clips","prompt":"a lake"})).await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(
        (a["seconds"].clone(), a["size"].clone()),
        (json!("4"), Value::Null)
    );
    // A named length and size are sent as named.
    let (status, b) =
        create(json!({"model":"company/clips","prompt":"a lake","seconds":8,"size":"1280x720"}))
            .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let (status, c) = create(json!({"model":"company/clips","prompt":"a slow lake"})).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    {
        let forms = mock.forms.lock().unwrap();
        assert_eq!(field(&forms[0], "seconds").as_deref(), Some("4"));
        assert_eq!(field(&forms[0], "size"), None, "no size named, none sent");
        assert_eq!(field(&forms[0], "model").as_deref(), Some("clip-1"));
        assert_eq!(field(&forms[1], "seconds").as_deref(), Some("8"));
        assert_eq!(field(&forms[1], "size").as_deref(), Some("1280x720"));
    }
    let job = |v: &Value| {
        Uuid::parse_str(v["id"].as_str().unwrap().strip_prefix("video_").unwrap()).unwrap()
    };
    let (a_id, b_id, c_id) = (job(&a), job(&b), job(&c));
    let execution = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>("SELECT execution_id FROM async_jobs WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    // A and B finish, and their first downloads fail; C never finishes.
    set_status(&mock, "vid_1", "completed");
    set_status(&mock, "vid_2", "completed");
    set_status(&mock, "vid_3", "in_progress");
    mock.content_fails.store(true, Ordering::SeqCst);
    let due = async || {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(&pool)
            .await
            .unwrap();
    };
    due().await;
    // The first poll settles A and B; their downloads fail.
    poller.poll_once().await.unwrap();
    let reservation = async |id: Uuid| {
        sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
            "SELECT state,actual_microusd,held_microusd FROM governance_reservations WHERE execution_id=$1",
        )
        .bind(execution(id).await)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // Settled from what the server reports: A at its own pick (1280x720,
    // 4 s × $0.20), B as named (8 s × $0.20).
    assert_eq!(reservation(a_id).await.1, Some(800_000));
    assert_eq!(reservation(b_id).await.1, Some(1_600_000));
    let content = |id: Uuid| get(format!("/v1/videos/video_{}/content", id.simple()));
    let response = content(a_id).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "video_not_ready");
    // The download works again: the next poll stores both, from then on
    // served from the gateway's copy.
    mock.content_fails.store(false, Ordering::SeqCst);
    due().await;
    poller.poll_once().await.unwrap();
    for id in [a_id, b_id] {
        let response = content(id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "video/mp4");
        assert_eq!(&response.bytes().await.unwrap()[..], MP4);
    }
    let downloads = mock.contents.load(Ordering::SeqCst);
    due().await;
    poller.poll_once().await.unwrap();
    assert_eq!(content(a_id).await.status(), StatusCode::OK);
    assert_eq!(
        mock.contents.load(Ordering::SeqCst),
        downloads,
        "stored once, never downloaded again"
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM stored_files f JOIN async_jobs j ON j.output_file_id=f.id WHERE f.purpose='video_output' AND f.workspace_id=j.workspace_id AND f.deleted_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored, 2,
        "each finished video stored once, in its workspace"
    );

    // C never finishes: past its poll deadline it is no longer polled, and
    // lease reconciliation keeps its hold as unknown.
    sqlx::query("UPDATE async_jobs SET poll_deadline_at=now()-interval '1 minute' WHERE id=$1")
        .bind(c_id)
        .execute(&pool)
        .await
        .unwrap();
    due().await;
    let retrieves = mock.retrieves.load(Ordering::SeqCst);
    poller.poll_once().await.unwrap();
    assert_eq!(
        mock.retrieves.load(Ordering::SeqCst),
        retrieves,
        "nothing polled past the deadline"
    );
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE execution_id=$1")
        .bind(execution(c_id).await)
        .execute(&pool)
        .await
        .unwrap();
    while crate::governance::reconcile_expired(&f.store, 10)
        .await
        .unwrap()
        > 0
    {}
    let (state, actual, held) = reservation(c_id).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0), "the hold is kept");

    // A server without a delete (405): the record goes,
    // and the answer says the provider's copy stays.
    let response = client
        .delete(format!("{gateway}/v1/videos/video_{}", a_id.simple()))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: Value = response.json().await.unwrap();
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["upstream_deleted"], false);
    assert_eq!(mock.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(content(a_id).await.status(), StatusCode::NOT_FOUND);
    let read = get(format!("/v1/videos/video_{}", a_id.simple())).await;
    assert_eq!(
        read.status(),
        StatusCode::NOT_FOUND,
        "a deleted video is gone"
    );
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM stored_files f JOIN async_jobs j ON j.output_file_id=f.id WHERE j.id=$1 AND f.deleted_at IS NULL AND (f.expires_at IS NULL OR f.expires_at>now())")
        .bind(a_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 0, "the gateway's copy is gone");
}

/// One upstream object is never reachable through two jobs: a server that
/// answers two creates (two routes of one connection, two workspaces) with
/// one id gets it recorded for the first only; the second job keeps no id,
/// so it can neither read nor delete the first's video. A delete reports
/// the provider's copy as gone only when the create is known to have left
/// none.
#[dbtest::test]
async fn one_upstream_video_belongs_to_one_job(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let mock = Arc::new(Contract::default());
    let (upstream, _upstream) = serve(contract(mock.clone())).await;
    let base = format!("{upstream}/v1");
    // Two connection records to one server, its base spelt with and
    // without a trailing slash: one account.
    let (connection, other_record) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name, endpoint) in [
        (connection, "Clip server", base.clone()),
        (other_record, "Clip server again", format!("{base}/")),
    ] {
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,$2,'openai_compatible','none',$3,true)")
            .bind(id).bind(name).bind(endpoint).execute(&pool).await.unwrap();
    }
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.push(line(Meter::OutputVideoSecondsMs, "100000", 1000));
    // One model per workspace, each with its own route, on its own record.
    for (name, workspace, record) in [
        ("company/mine", f.principal.workspace_id, connection),
        ("company/theirs", f.team.workspace_id, other_record),
    ] {
        let (model, route) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query(
            "INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY['videos'])",
        )
        .bind(model)
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')")
            .bind(workspace).bind(model).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds) VALUES($1,$2,$3,'clip-1',true,'{4}')")
            .bind(route).bind(model).bind(record).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
            .bind(Uuid::new_v4()).bind(route).bind(json!(lines)).execute(&pool).await.unwrap();
    }
    let registry = || {
        let mut registry = ProviderRegistry::default();
        for adapter in crate::providers::local::adapters(
            Arc::new(EnvSecrets::new([])),
            ApprovedEndpoints::for_test(&base),
        ) {
            registry.register(adapter).unwrap();
        }
        registry
    };
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry(),
        EngineLimits::default(),
    )
    .unwrap();
    sqlx::query("UPDATE installation_settings SET file_video_enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    let files = test_files().await;
    let (gateway, _gateway) = serve(
        crate::http::router_with_engine(f.store.clone(), None, engine)
            .layer(axum::Extension(files.clone())),
    )
    .await;
    // The poller: it observes a finished video and stores its output (until
    // then the video reads in progress).
    let poller = Jobs::with(
        f.store.clone(),
        registry(),
        JobLimits {
            poll_interval: Some(std::time::Duration::from_secs(1)),
            ..JobLimits::default()
        },
    )
    .with_files(Some(files));
    let stored = async || {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(&pool)
            .await
            .unwrap();
        let _ = poller.poll_once().await;
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (mine, theirs) = (
        bearer(&f, f.principal.workspace_id).await,
        bearer(&f, f.team.workspace_id).await,
    );
    let create = async |auth: &str, model: &str| {
        let response = client
            .post(format!("{gateway}/v1/videos"))
            .header("authorization", auth)
            .json(&json!({"model": model, "prompt": "a lake"}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            response.json::<Value>().await.unwrap_or(Value::Null),
        )
    };
    // A route whose price has no video rate: refused before admission,
    // nothing sent, no job.
    let (model, route) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/unpriced',ARRAY['videos'])")
        .bind(model).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(f.principal.workspace_id)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds) VALUES($1,$2,$3,'clip-1',true,'{4}')")
        .bind(route).bind(model).bind(connection).execute(&pool).await.unwrap();
    let mut tokens_only = lines.clone();
    tokens_only.pop();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(route).bind(json!(tokens_only)).execute(&pool).await.unwrap();
    let (status, body) = create(&mine, "company/unpriced").await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("video_price_missing")),
        "{body}"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (jobs, mock.forms.lock().unwrap().len()),
        (0, 0),
        "nothing sent, no job"
    );
    *mock.fixed_id.lock().unwrap() = Some("vid_same".into());
    let (status, first) = create(&mine, "company/mine").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, body) = create(&theirs, "company/theirs").await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (StatusCode::BAD_GATEWAY, json!("invalid_upstream_response")),
        "the second create is not recorded: {body}"
    );
    let recorded: Vec<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT workspace_id,upstream_id FROM async_jobs ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        recorded,
        [
            (f.principal.workspace_id, Some("vid_same".into())),
            (f.team.workspace_id, None)
        ]
    );
    // The second job ends at once without an id, its hold kept; deleting
    // it says the provider's copy may remain, and sends nothing.
    let (second, state, code, held): (Uuid, String, String, String) = sqlx::query_as(
        "SELECT j.id,j.state,j.error_code,r.state FROM async_jobs j JOIN governance_reservations r ON r.execution_id=j.execution_id WHERE j.workspace_id=$1",
    )
    .bind(f.team.workspace_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (state.as_str(), code.as_str(), held.as_str()),
        ("failed", "upstream_id_reused", "unknown")
    );
    let response = client
        .delete(format!("{gateway}/v1/videos/video_{}", second.simple()))
        .header("authorization", &theirs)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: Value = response.json().await.unwrap();
    assert_eq!(
        deleted["upstream_deleted"], false,
        "the provider may still have it"
    );
    assert_eq!(
        mock.deletes.load(Ordering::SeqCst),
        0,
        "nothing sent for a job without an id"
    );
    // The server gave the first job's id to the second create, so it may
    // hold another workspace's video behind it: the first job is never asked
    // about again (its content is an upstream error, its delete local only).
    set_status(&mock, "vid_same", "completed");
    let path = format!("{gateway}/v1/videos/{}", first["id"].as_str().unwrap());
    let reused: String =
        sqlx::query_scalar("SELECT error_code FROM async_jobs WHERE upstream_id='vid_same'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reused, "upstream_id_reused");
    let (contents, retrieves) = (
        mock.contents.load(Ordering::SeqCst),
        mock.retrieves.load(Ordering::SeqCst),
    );
    // It reads failed with its error, and settles with its hold kept as
    // unknown cost, without the server being asked.
    let read = client
        .get(&path)
        .header("authorization", &mine)
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    let read: Value = read.json().await.unwrap();
    assert_eq!(
        (read["status"].clone(), read["error"]["code"].clone()),
        (json!("failed"), json!("upstream_id_reused")),
        "{read}"
    );
    let (held, settled): (String, bool) = sqlx::query_as("SELECT r.state,j.settled_at IS NOT NULL FROM async_jobs j JOIN governance_reservations r ON r.execution_id=j.execution_id WHERE j.upstream_id='vid_same'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((held.as_str(), settled), ("unknown", true));
    let deleted: Value = client
        .delete(&path)
        .header("authorization", &mine)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            deleted["deleted"].clone(),
            deleted["upstream_deleted"].clone()
        ),
        (json!(true), json!(false))
    );
    assert_eq!(
        (
            mock.contents.load(Ordering::SeqCst),
            mock.retrieves.load(Ordering::SeqCst),
            mock.deletes.load(Ordering::SeqCst)
        ),
        (contents, retrieves, 0),
        "nothing sent for a reused id"
    );
    // A fresh video: its content as the server labels it (a type outside
    // the allowlist as `application/octet-stream`), never sniffed; its
    // delete answered `404` (already gone) counts as gone.
    *mock.fixed_id.lock().unwrap() = None;
    let (status, fresh) = create(&mine, "company/mine").await;
    assert_eq!(status, StatusCode::OK, "{fresh}");
    let upstream: String = sqlx::query_scalar("SELECT upstream_id FROM async_jobs WHERE id=$1")
        .bind(
            Uuid::parse_str(
                fresh["id"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("video_")
                    .unwrap(),
            )
            .unwrap(),
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    set_status(&mock, &upstream, "completed");
    stored().await;
    let path = format!("{gateway}/v1/videos/{}", fresh["id"].as_str().unwrap());
    let read: Value = client
        .get(&path)
        .header("authorization", &mine)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["status"], "completed", "{read}");
    // A route without sizes under a flat price: the size the server reports
    // (1280x720) is billed at the line without a size (4 s × $0.10).
    let settled: (String, Option<i64>) = sqlx::query_as(
        "SELECT r.state,r.actual_microusd FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE j.upstream_id=$1",
    )
    .bind(&upstream)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled, ("settled".to_owned(), Some(400_000)));
    // A thumbnail comes from the server (the video itself only from the
    // gateway's stored copy), as labeled among the image types, a generic
    // binary label as `application/octet-stream`, never sniffed.
    for (media, served) in [
        ("image/png", "image/png"),
        ("image/webp", "image/webp"),
        ("application/binary", "application/octet-stream"),
    ] {
        *mock.content.lock().unwrap() = Some((200, media, MP4.to_vec(), None));
        let response = client
            .get(format!("{path}/content?variant=thumbnail"))
            .header("authorization", &mine)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{media}");
        assert_eq!(response.headers()["content-type"], served, "{media}");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
    *mock.delete_status.lock().unwrap() = Some(404);
    let deleted: Value = client
        .delete(&path)
        .header("authorization", &mine)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            deleted["deleted"].clone(),
            deleted["upstream_deleted"].clone()
        ),
        (json!(true), json!(true))
    );
    // A completed, settled video whose id the server then gives another
    // job: it reads failed from then on, its charge stands, and neither its
    // content nor the server is reached for it. Its content was first
    // refused for declaring more than the limit.
    *mock.fixed_id.lock().unwrap() = None;
    let (status, kept) = create(&mine, "company/mine").await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    let kept_id =
        Uuid::parse_str(kept["id"].as_str().unwrap().strip_prefix("video_").unwrap()).unwrap();
    let kept_upstream: String =
        sqlx::query_scalar("SELECT upstream_id FROM async_jobs WHERE id=$1")
            .bind(kept_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    set_status(&mock, &kept_upstream, "completed");
    let kept_path = format!("{gateway}/v1/videos/{}", kept["id"].as_str().unwrap());
    let get = async |path: &str, auth: &str| -> (StatusCode, Value) {
        let response = client
            .get(path)
            .header("authorization", auth)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    };
    stored().await;
    assert_eq!(get(&kept_path, &mine).await.1["status"], "completed");
    *mock.content.lock().unwrap() = Some((
        200,
        "image/png",
        MP4.to_vec(),
        Some(crate::jobs::content_bounds::VIDEO_ASSET_MAX_BYTES + 1),
    ));
    let (status, _) = get(&format!("{kept_path}/content?variant=thumbnail"), &mine).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "a declared length over the limit"
    );
    *mock.content.lock().unwrap() = None;
    *mock.fixed_id.lock().unwrap() = Some(kept_upstream.clone());
    let (status, body) = create(&theirs, "company/theirs").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let (calls, read) = (
        mock.retrieves.load(Ordering::SeqCst),
        get(&kept_path, &mine).await,
    );
    assert_eq!(
        (
            read.0,
            read.1["status"].clone(),
            read.1["error"]["code"].clone()
        ),
        (StatusCode::OK, json!("failed"), json!("upstream_id_reused")),
        "{}",
        read.1
    );
    let charged: (String, Option<i64>) = sqlx::query_as("SELECT r.state,r.actual_microusd FROM async_jobs j JOIN governance_reservations r ON r.execution_id=j.execution_id WHERE j.id=$1")
        .bind(kept_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        charged,
        ("settled".to_owned(), Some(400_000)),
        "the charge stands"
    );
    let (status, body) = get(&format!("{kept_path}/content"), &mine).await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("video_not_ready"))
    );
    assert_eq!(
        mock.retrieves.load(Ordering::SeqCst),
        calls,
        "nothing asked"
    );
    // The deleted, settled fresh video's id may be given out again: the new
    // job records it. A delete answered `204` counts as deleted.
    *mock.fixed_id.lock().unwrap() = Some(upstream.clone());
    let (status, reissued) = create(&theirs, "company/theirs").await;
    assert_eq!(status, StatusCode::OK, "{reissued}");
    set_status(&mock, &upstream, "completed");
    let reissued_path = format!("{gateway}/v1/videos/{}", reissued["id"].as_str().unwrap());
    stored().await;
    assert_eq!(get(&reissued_path, &theirs).await.1["status"], "completed");
    *mock.delete_status.lock().unwrap() = Some(204);
    let deleted: Value = client
        .delete(&reissued_path)
        .header("authorization", &theirs)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["upstream_deleted"], json!(true), "{deleted}");
}
