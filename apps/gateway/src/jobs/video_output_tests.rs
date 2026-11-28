//! #52 on the real path (a running gateway, the job poller, a mock server of
//! the OpenAI video contract, PostgreSQL): what happens to a completed
//! video's output — stored, given up for good (the job stays charged),
//! retried with backoff under a kept lease, expired, bounded and fenced —
//! and what `/content` and a create answer for it.
use super::db_tests::{bearer, line, na, test_files};
use super::video_contract_tests::{Contract, MP4, contract, set_status};
use super::*;
use crate::{
    billing::v3::Meter,
    governance::tests::db::{Fixture, fixture},
    inference::{
        Engine, EngineLimits,
        admission_validation_tests::{Server, serve},
    },
    providers::{ProviderRegistry, local::endpoints::ApprovedEndpoints, secrets::EnvSecrets},
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, atomic::Ordering};

/// A route of a test model: its lengths and sizes (SQL array literals).
struct Route {
    seconds: &'static str,
    sizes: Option<&'static str>,
}
const fn route(seconds: &'static str, sizes: Option<&'static str>) -> Route {
    Route { seconds, sizes }
}

struct Setup {
    pool: PgPool,
    f: Fixture,
    mock: Arc<Contract>,
    gateway: String,
    /// The same gateway without the file store.
    bare: String,
    poller: Jobs,
    auth: String,
    client: reqwest::Client,
    connection: Uuid,
    _servers: [Server; 3],
}

impl Setup {
    async fn new(pool: PgPool) -> Self {
        let f = fixture(pool.clone()).await;
        let mock = Arc::new(Contract::default());
        let (upstream, upstream_server) = serve(contract(mock.clone())).await;
        let base = format!("{upstream}/v1");
        let connection = Uuid::new_v4();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Clip server','openai_compatible','none',$2,true)")
            .bind(connection).bind(&base).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('personal',20) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs")
            .execute(&pool).await.unwrap();
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
        let engine = || {
            Engine::new(
                Arc::new(f.store.clone()),
                registry(),
                EngineLimits::default(),
            )
            .unwrap()
        };
        let (gateway, gateway_server) = serve(
            crate::http::router_with_engine(f.store.clone(), None, engine())
                .layer(axum::Extension(files.clone())),
        )
        .await;
        let (bare, bare_server) = serve(crate::http::router_with_engine(
            f.store.clone(),
            None,
            engine(),
        ))
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
        Self {
            pool,
            f,
            mock,
            gateway,
            bare,
            poller,
            auth,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            connection,
            _servers: [upstream_server, gateway_server, bare_server],
        }
    }

    /// A video model offered to the workspace, with these routes (in order).
    async fn model(&self, name: &str, routes: &[Route]) -> Vec<Uuid> {
        let model = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY['videos'])",
        )
        .bind(model)
        .bind(name)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')")
            .bind(self.f.principal.workspace_id).bind(model).execute(&self.pool).await.unwrap();
        let mut ids = Vec::new();
        for r in routes {
            // $0.10/s without a size, $0.20/s at 1280x720.
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
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds,video_sizes) VALUES($1,$2,$3,'clip-1',$4,$5::integer[],$6::text[])")
                .bind(id).bind(model).bind(self.connection).bind(true).bind(r.seconds).bind(r.sizes)
                .execute(&self.pool).await.unwrap();
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
                .bind(Uuid::new_v4()).bind(id).bind(json!(lines)).execute(&self.pool).await.unwrap();
            ids.push(id);
        }
        ids
    }

    async fn create(&self, body: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/v1/videos", self.gateway))
            .header("authorization", &self.auth)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap())
    }
    async fn call(&self, method: reqwest::Method, base: &str, path: &str) -> reqwest::Response {
        self.client
            .request(method, format!("{base}{path}"))
            .header("authorization", &self.auth)
            .send()
            .await
            .unwrap()
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.call(reqwest::Method::GET, &self.gateway, path).await
    }
    async fn content(&self, job: Uuid) -> reqwest::Response {
        self.get(&format!("/v1/videos/video_{}/content", job.simple()))
            .await
    }
    /// Every job due now, then one poll pass.
    async fn poll(&self) {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(&self.pool)
            .await
            .unwrap();
        let _ = self.poller.poll_once().await;
    }
    /// Create a video, have the server finish it and poll once.
    async fn finished(&self, model: &str) -> Uuid {
        let (status, v) = self.create(json!({"model":model,"prompt":"a lake"})).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let id =
            Uuid::parse_str(v["id"].as_str().unwrap().strip_prefix("video_").unwrap()).unwrap();
        let upstream: String = sqlx::query_scalar("SELECT upstream_id FROM async_jobs WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        set_status(&self.mock, &upstream, "completed");
        self.poll().await;
        id
    }
    async fn job(&self, id: Uuid) -> (Option<String>, Option<Uuid>, i32, bool) {
        sqlx::query_as("SELECT output_failure,output_file_id,poll_failures,next_poll_at>now() FROM async_jobs WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    /// The job's reservation: its state and its settled cost.
    async fn charge(&self, id: Uuid) -> (String, Option<i64>) {
        sqlx::query_as("SELECT r.state,r.actual_microusd FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE j.id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    fn forms(&self) -> usize {
        self.mock.forms.lock().unwrap().len()
    }
}

fn job_id(v: &Value) -> Uuid {
    Uuid::parse_str(v["id"].as_str().unwrap().strip_prefix("video_").unwrap()).unwrap()
}

#[dbtest::test]
async fn an_output_that_cannot_be_stored_is_given_up_for_good_and_the_job_stays_charged(
    pool: PgPool,
) {
    let s = Setup::new(pool.clone()).await;
    s.model("company/clips", &[route("{4}", Some("{1280x720}"))])
        .await;
    let mp4 = MP4.to_vec();
    for (content, reason) in [
        ((200, "video/mp4", Vec::new(), None), "empty_output"),
        (
            (200, "video/mp4", mp4.clone(), Some(600 * 1024 * 1024)),
            "too_large",
        ),
    ] {
        *s.mock.content.lock().unwrap() = Some(content);
        let id = s.finished("company/clips").await;
        let (failure, file, _, _) = s.job(id).await;
        assert_eq!((failure.as_deref(), file), (Some(reason), None), "{reason}");
        // Charged as the server reported it (4 s at 1280x720, $0.20/s).
        assert_eq!(
            s.charge(id).await,
            ("settled".into(), Some(800_000)),
            "{reason}"
        );
        let response = s.content(id).await;
        assert_eq!(response.status(), StatusCode::GONE, "{reason}");
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["code"], "video_output_unavailable");
        assert_eq!(error["error"]["reason"], reason);
        let v: Value = s
            .get(&format!("/v1/videos/video_{}", id.simple()))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["output_unavailable"], json!({"reason": reason}));
        // Given up for good: never downloaded again.
        let downloads = s.mock.contents.load(Ordering::SeqCst);
        s.poll().await;
        assert_eq!(
            s.mock.contents.load(Ordering::SeqCst),
            downloads,
            "{reason}"
        );
    }
    *s.mock.content.lock().unwrap() = None;

    // Video outputs turned off after the job was created.
    let (status, v) = s
        .create(json!({"model":"company/clips","prompt":"a"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    sqlx::query("UPDATE installation_settings SET file_video_enabled=false WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    let upstream: String = sqlx::query_scalar("SELECT upstream_id FROM async_jobs WHERE id=$1")
        .bind(job_id(&v))
        .fetch_one(&pool)
        .await
        .unwrap();
    set_status(&s.mock, &upstream, "completed");
    s.poll().await;
    assert_eq!(
        s.job(job_id(&v)).await.0.as_deref(),
        Some("storage_disabled")
    );
    sqlx::query("UPDATE installation_settings SET file_video_enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();

    // A download that keeps failing, or that the server keeps refusing (its
    // content endpoint may lag the status), backs off after each attempt and
    // is given up after six, with the last attempt's cause.
    for (fail, reason) in [
        (None, "download_failed"),
        (
            Some((404, "video/mp4", Vec::new(), None)),
            "upstream_refused",
        ),
    ] {
        s.mock.content_fails.store(fail.is_none(), Ordering::SeqCst);
        *s.mock.content.lock().unwrap() = fail;
        let id = s.finished("company/clips").await;
        for attempt in 1..super::video_output::VIDEO_OUTPUT_ATTEMPTS {
            let (failure, _, failures, later) = s.job(id).await;
            assert_eq!(
                (failure, failures, later),
                (None, attempt, true),
                "{reason}: attempt {attempt}"
            );
            s.poll().await;
        }
        assert_eq!(s.job(id).await.0.as_deref(), Some(reason));
        assert_eq!(s.charge(id).await.1, Some(800_000));
    }
}

#[dbtest::test]
async fn the_claim_s_lease_is_kept_while_an_output_is_stored(pool: PgPool) {
    let s = Setup::new(pool.clone()).await;
    s.model("company/clips", &[route("{4}", Some("{1280x720}"))])
        .await;
    s.mock.content_fails.store(true, Ordering::SeqCst);
    let id = s.finished("company/clips").await;
    // Settled and awaiting its output: a claim takes it, and its renewal is
    // held (not "settled"), so no other poller takes it meanwhile.
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (job, fence) = store::claim_due(&s.f.store, Uuid::new_v4(), 1.0, 60.0)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(job.id, id);
    assert!(job.settled_at.is_some());
    assert!(matches!(
        fence.renew(&s.f.store, 60.0).await.unwrap(),
        super::fence::Renewed::Held { .. }
    ));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store::claim_due(&s.f.store, Uuid::new_v4(), 1.0, 60.0)
            .await
            .unwrap()
            .is_none(),
        "a second poller does not take it while the lease runs"
    );
    // Stored under that claim, the renewal reports the job finished.
    s.mock.content_fails.store(false, Ordering::SeqCst);
    s.poller
        .refresh_video(job, store::Observer::Claim(&fence))
        .await
        .unwrap();
    assert!(s.job(id).await.1.is_some(), "stored");
    assert_eq!(
        fence.renew(&s.f.store, 60.0).await.unwrap(),
        super::fence::Renewed::Settled
    );
}

/// A create needs the file store accepting video outputs and the workspace
/// under its storage quota; `/content` serves the stored copy, and answers
/// for each state it may be in: not stored yet (400), never to be (410 with
/// a reason, `completed_before_storage` for a video completed before outputs
/// were stored),
/// expired (410), or the store unavailable (503). A job whose provider id
/// the server gave another job has its awaited output given up, never
/// fetched.
#[dbtest::test]
async fn content_and_create_answer_for_the_stored_copy(pool: PgPool) {
    let s = Setup::new(pool.clone()).await;
    s.model("company/clips", &[route("{4}", Some("{1280x720}"))])
        .await;
    let body = json!({"model":"company/clips","prompt":"a lake"});
    // No file store: refused, nothing sent, no job.
    let response = s
        .client
        .post(format!("{}/v1/videos", s.bare))
        .header("authorization", &s.auth)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "video_storage_unavailable");
    assert_eq!(s.forms(), 0, "nothing sent");

    // Stored: served from the gateway as received.
    let id = s.finished("company/clips").await;
    let (_, file, _, _) = s.job(id).await;
    let file = file.expect("stored");
    let response = s.content(id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "video/mp4");
    assert_eq!(&response.bytes().await.unwrap()[..], MP4);
    // Stored as the server labels it among the video types; any other label
    // (none, generic, or not a video type) as `application/octet-stream`,
    // never sniffed. The stored file carries the key's holder, as the job's
    // creator, for visibility, legal holds and subject exports.
    for (media, stored) in [
        ("video/webm", "video/webm"),
        ("video/quicktime", "video/quicktime"),
        ("application/binary", "application/octet-stream"),
        ("text/html", "application/octet-stream"),
    ] {
        *s.mock.content.lock().unwrap() = Some((200, media, MP4.to_vec(), None));
        let id = s.finished("company/clips").await;
        let response = s.content(id).await;
        assert_eq!(response.status(), StatusCode::OK, "{media}");
        assert_eq!(response.headers()["content-type"], stored, "{media}");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let creator: (Option<Uuid>, Option<Uuid>) = sqlx::query_as("SELECT f.created_by_user_id,f.created_by_api_key_id FROM stored_files f JOIN async_jobs j ON j.output_file_id=f.id WHERE j.id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let key: (Uuid, Option<Uuid>) = sqlx::query_as("SELECT j.api_key_id,k.issued_to_user_id FROM async_jobs j JOIN api_keys k ON k.id=j.api_key_id WHERE j.id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(key.1.is_some(), "a member's key");
        assert_eq!(creator, (key.1, Some(key.0)), "{media}");
    }
    *s.mock.content.lock().unwrap() = None;
    // The store unavailable (the gateway without it): 503, never another
    // download.
    let downloads = s.mock.contents.load(Ordering::SeqCst);
    let response = s
        .call(
            reqwest::Method::GET,
            &s.bare,
            &format!("/v1/videos/video_{}/content", id.simple()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    // Expired by retention: 410.
    sqlx::query("UPDATE stored_files SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    let response = s.content(id).await;
    assert_eq!(response.status(), StatusCode::GONE);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "video_output_expired");
    assert_eq!(s.mock.contents.load(Ordering::SeqCst), downloads);

    // Not stored yet: 400, try again later.
    s.mock.content_fails.store(true, Ordering::SeqCst);
    let pending = s.finished("company/clips").await;
    let response = s.content(pending).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "video_content_unavailable");
    // The server gives that video's id to another job: the awaited output
    // is given up at the next claim, without a download.
    let upstream: String = sqlx::query_scalar("SELECT upstream_id FROM async_jobs WHERE id=$1")
        .bind(pending)
        .fetch_one(&pool)
        .await
        .unwrap();
    s.mock.content_fails.store(false, Ordering::SeqCst);
    *s.mock.fixed_id.lock().unwrap() = Some(upstream);
    let (status, _) = s.create(body.clone()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    *s.mock.fixed_id.lock().unwrap() = None;
    let downloads = s.mock.contents.load(Ordering::SeqCst);
    s.poll().await;
    assert_eq!(
        s.job(pending).await.0.as_deref(),
        Some("upstream_id_reused")
    );
    assert_eq!(
        s.mock.contents.load(Ordering::SeqCst),
        downloads,
        "never fetched"
    );
    assert_eq!(
        s.charge(pending).await.1,
        Some(800_000),
        "its charge stands"
    );

    // Completed as a release without stored outputs completed it (no
    // awaited output): 410, `completed_before_storage`.
    let (status, v) = s.create(body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let old = job_id(&v);
    sqlx::query("UPDATE async_jobs SET state='completed',upstream_status='completed',completed_at=now() WHERE id=$1")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    let response = s.content(old).await;
    assert_eq!(response.status(), StatusCode::GONE);
    let error: Value = response.json().await.unwrap();
    assert_eq!(
        (
            error["error"]["code"].clone(),
            error["error"]["reason"].clone()
        ),
        (
            json!("video_output_unavailable"),
            json!("completed_before_storage")
        )
    );

    // At or over the storage quota (one stored video over a 1-byte quota):
    // no new video.
    let stored = s.finished("company/clips").await;
    assert!(s.job(stored).await.1.is_some());
    sqlx::query("UPDATE workspace_type_policies SET storage_bytes=1 WHERE kind='personal'")
        .execute(&pool)
        .await
        .unwrap();
    let sent = s.forms();
    let (status, error) = s.create(body).await;
    assert_eq!(
        (status, error["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("storage_quota_exceeded")),
        "{error}"
    );
    assert_eq!(s.forms(), sent, "nothing sent");
}

/// What bounds and fences a stored copy: a video awaiting its output holds
/// the largest output against the quota; only a live video output of the
/// job's own workspace is linked, never once the job is marked
/// `upstream_id_reused`; a poller whose claim ended stores nothing; and a
/// completed video whose settlement never landed is still claimed for its
/// output once its poll deadline has passed.
#[dbtest::test]
async fn a_stored_copy_is_bounded_and_fenced(pool: PgPool) {
    let s = Setup::new(pool.clone()).await;
    s.model("company/clips", &[route("{4}", Some("{1280x720}"))])
        .await;
    let body = json!({"model":"company/clips","prompt":"a lake"});
    // One video awaiting its output under a 100 MiB quota: it holds 512 MiB,
    // so no new video until it is stored.
    s.mock.content_fails.store(true, Ordering::SeqCst);
    let awaiting = s.finished("company/clips").await;
    sqlx::query("UPDATE workspace_type_policies SET storage_bytes=104857600 WHERE kind='personal'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, error) = s.create(body.clone()).await;
    assert_eq!(
        (status, error["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("storage_quota_exceeded")),
        "{error}"
    );
    s.mock.content_fails.store(false, Ordering::SeqCst);
    s.poll().await;
    let stored = s.job(awaiting).await.1.expect("stored");
    let (status, v) = s.create(body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    sqlx::query("UPDATE workspace_type_policies SET storage_bytes=NULL WHERE kind='personal'")
        .execute(&pool)
        .await
        .unwrap();

    // A claimed video awaiting its output: what may be linked to it.
    s.mock.content_fails.store(true, Ordering::SeqCst);
    let pending = s.finished("company/clips").await;
    s.mock.content_fails.store(false, Ordering::SeqCst);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    let (job, fence) = store::claim_due(&s.f.store, Uuid::new_v4(), 1.0, 60.0)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(job.id, pending);
    // Another job's file that is no longer live, then a live one once the
    // job is marked: neither is linked.
    sqlx::query("UPDATE stored_files SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(stored)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !store::set_video_output(&s.f.store, &fence, pending, stored)
            .await
            .unwrap(),
        "an expired file"
    );
    let live = s.finished("company/clips").await;
    let live = s.job(live).await.1.expect("stored");
    sqlx::query("UPDATE async_jobs SET error_code='upstream_id_reused' WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !store::set_video_output(&s.f.store, &fence, pending, live)
            .await
            .unwrap(),
        "a marked job"
    );
    sqlx::query("UPDATE async_jobs SET error_code=NULL WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    // A claim already lost: the download stops at its first chunk and no
    // copy is kept or linked.
    let outputs = async || -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE purpose='video_output' AND committed_at IS NOT NULL AND deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = outputs().await;
    let refreshed = s
        .poller
        .refresh_video_until(
            job,
            store::Observer::Claim(&fence),
            &std::sync::atomic::AtomicBool::new(true),
        )
        .await;
    assert!(refreshed.is_err(), "retried later");
    assert_eq!(s.job(pending).await.1, None);
    assert_eq!(outputs().await, before);

    // Completed and awaiting its output, never settled, past its poll
    // deadline: still claimed, for its output.
    let (status, v) = s.create(body).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let unsettled = job_id(&v);
    sqlx::query("UPDATE async_jobs SET state='completed',upstream_status='completed',completed_at=now(),output_pending=true,poll_deadline_at=now()-interval '1 second',next_poll_at=now() WHERE id=$1")
        .bind(unsettled)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()+interval '1 hour' WHERE id<>$1")
        .bind(unsettled)
        .execute(&pool)
        .await
        .unwrap();
    let (claimed, _) = store::claim_due(&s.f.store, Uuid::new_v4(), 1.0, 60.0)
        .await
        .unwrap()
        .expect("claimed for its output");
    assert_eq!((claimed.id, claimed.settled_at), (unsettled, None));
}
