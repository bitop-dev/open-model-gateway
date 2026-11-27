//! Async jobs on a disposable database with a fake provider: the state
//! machine, reservation/settlement exactness, ownership/privacy, cancel,
//! expiry, unknown usage, the streamed upload contract and idempotent polling.
use super::*;
use crate::{
    billing::{MeterVariant, v3::Meter},
    governance::tests::db::{Fixture, amounts, fixture},
};
use async_trait::async_trait;
use gateway_datetime_compat::UtcInstant;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Mutex;

#[derive(Default)]
struct Script {
    video_hold: Option<Arc<FinalizeHold>>,
    /// The provider makes a create with the same key the same job.
    idempotent: bool,
    /// The idempotency key of every create call, in order.
    keys: Vec<String>,
    /// Scripted create outcomes, before `videos` is consulted: `Some(true)`
    /// fails before any request is built (nothing sent), `Some(false)` fails
    /// after it was sent, each with the paired error.
    create_failures: std::collections::VecDeque<(bool, InferenceError)>,
    videos: std::collections::VecDeque<UpstreamVideo>,
    batches: std::collections::VecDeque<UpstreamBatch>,
    calls: Vec<&'static str>,
}
#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Script>>);
impl Fake {
    pub(crate) fn push_video(&self, v: UpstreamVideo) {
        self.0.lock().unwrap().videos.push_back(v);
    }
    fn calls(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().calls.clone()
    }
    fn call(&self, name: &'static str) {
        self.0.lock().unwrap().calls.push(name);
    }
    fn next_video(&self) -> Result<UpstreamVideo, InferenceError> {
        self.0
            .lock()
            .unwrap()
            .videos
            .pop_front()
            .ok_or(InferenceError::UpstreamUnavailable)
    }
    fn next_batch(&self) -> Result<UpstreamBatch, InferenceError> {
        self.0
            .lock()
            .unwrap()
            .batches
            .pop_front()
            .ok_or(InferenceError::UpstreamUnavailable)
    }
}
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> crate::inference::types::Capabilities {
        crate::inference::types::Capabilities {
            image_input: false,
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, p: ApiProtocol) -> bool {
        matches!(p, ApiProtocol::Videos | ApiProtocol::Batches)
    }
    fn supports_video_request(&self, _: &Deployment, _: &VideoRequest) -> bool {
        true
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: crate::inference::types::ChatRequest,
    ) -> Result<crate::inference::types::ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    fn idempotent_video_create(&self, _: &Deployment) -> bool {
        self.0.lock().unwrap().idempotent
    }
    async fn create_video_keyed(
        &self,
        target: &Deployment,
        request: VideoRequest,
        key: &VideoCreateKey,
    ) -> Result<UpstreamVideo, InferenceError> {
        let failure = {
            let mut script = self.0.lock().unwrap();
            script.keys.push(key.as_str().to_owned());
            script.create_failures.pop_front()
        };
        match failure {
            Some((true, e)) => {
                self.call("create_video");
                Err(e)
            }
            Some((false, e)) => {
                // Stands for a request that was built and sent.
                crate::inference::dispatch::started();
                self.call("create_video");
                Err(e)
            }
            None => self.create_video(target, request).await,
        }
    }
    async fn create_video(
        &self,
        target: &Deployment,
        request: VideoRequest,
    ) -> Result<UpstreamVideo, InferenceError> {
        assert_eq!(target.upstream_model, "mock-model");
        assert_eq!(request.prompt, "a calm lake");
        // Stands for an upstream call: marked like a real request builder.
        crate::inference::dispatch::started();
        self.call("create_video");
        let hold = self.0.lock().unwrap().video_hold.clone();
        if let Some(hold) = hold {
            hold.pause().await;
        }
        // Providers mint a unique id per job.
        self.next_video().map(|mut v| {
            v.id = UpstreamId::parse(&format!("video_{}", Uuid::new_v4().simple())).unwrap();
            v
        })
    }
    async fn retrieve_video(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamVideo, InferenceError> {
        self.call("retrieve_video");
        self.next_video()
    }
    async fn delete_video(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<VideoDeletion, InferenceError> {
        self.call("delete_video");
        Ok(VideoDeletion::Deleted)
    }
    async fn retrieve_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        self.call("retrieve_batch");
        self.next_batch()
    }
    async fn cancel_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        self.call("cancel_batch");
        self.next_batch()
    }
}

pub(crate) fn jobs(f: &Fixture, fake: &Fake, poll: bool) -> Jobs {
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake.clone())).unwrap();
    Jobs::with(
        f.store.clone(),
        registry,
        JobLimits {
            poll_interval: poll.then(|| Duration::from_secs(1)),
            ..JobLimits::default()
        },
    )
}
pub(crate) fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
pub(crate) fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// Per-second video price: 720p sizes $0.10/s, the larger sizes $0.50/s; tokens NA. The
/// route takes 4, 8 and 12 seconds (4 by default) and the four Sora sizes
/// (its video options, 0097).
pub(crate) async fn video_setup(f: &Fixture) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['videos'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET video_seconds='{4,8,12}',video_sizes='{720x1280,1280x720,1024x1792,1792x1024}' WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    // Every size the route lists has a rate (a video route must price each).
    for (size, rate) in [
        ("720x1280", "100000"),
        ("1280x720", "100000"),
        ("1024x1792", "500000"),
        ("1792x1024", "500000"),
    ] {
        let mut v = line(Meter::OutputVideoSecondsMs, rate, 1000);
        v["variant"] = json!(size);
        lines.push(v);
    }
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
}
pub(crate) fn video_request() -> VideoRequest {
    VideoRequest {
        model: "company/smart".into(),
        prompt: "a calm lake".into(),
        seconds: Some(8),
        size: Some("720x1280".into()),
    }
}
pub(crate) fn upstream_video(state: JobState, seconds: Option<u32>) -> UpstreamVideo {
    UpstreamVideo {
        id: UpstreamId::parse("video_up1").unwrap(),
        state,
        progress: None,
        seconds,
        size: MeterVariant::new("720x1280"),
        completed_at: None,
        expires_at: None,
        error: None,
    }
}
async fn lease(f: &Fixture, id: Uuid) -> gateway_datetime_compat::UtcInstant {
    sqlx::query_scalar("SELECT lease_expires_at FROM governance_reservations WHERE execution_id=$1")
        .bind(id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn video_reserves_highest_tier_and_settles_actual_seconds_once(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), request_id)
        .await
        .unwrap();
    assert_eq!(
        (job.state.as_str(), job.execution_id),
        ("queued", request_id)
    );
    // 8 s × the highest variant rate ($0.50/s), never the requested tier only.
    assert_eq!(
        amounts(&f, request_id).await,
        ("pending".into(), Some(4_000_000), None, false)
    );
    assert!(lease(&f, request_id).await > UtcInstant::now() + TimeDelta::hours(5));
    // Poll: in progress, then completed with the actual duration and tier.
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    let mut done = upstream_video(JobState::Completed, Some(8));
    done.progress = Some(100);
    fake.push_video(done);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, request_id).await,
        ("settled".into(), Some(4_000_000), Some(800_000), false)
    );
    let (state, meters, variant): (String, Value, String) = sqlx::query_as(
        "SELECT state,meter_usage,output_image_variant FROM inference_executions WHERE id=$1",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(state, "succeeded");
    assert_eq!(meters["output_video_seconds_ms"], "8000");
    assert_eq!(variant, "720x1280");
    let components: Value = sqlx::query_scalar(
        "SELECT cost_components FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(components["output_video_microusd"], "800000");
    // Settled jobs are never claimed again; a repeated settle is a no-op.
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 0);
    let row = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(row.state, "completed");
    assert!(row.settled_at.is_some());
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE execution_id=$1")
            .bind(request_id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(ledger, 2, "one hold and one settlement");
    assert_eq!(
        fake.calls(),
        ["create_video", "retrieve_video", "retrieve_video"]
    );
}

#[dbtest::test]
async fn unknown_or_failed_video_keeps_the_hold(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    // Completed without a reported duration: unknown, hold retained.
    fake.push_video(upstream_video(JobState::InProgress, None));
    let a = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), a)
        .await
        .unwrap();
    fake.push_video(upstream_video(JobState::Completed, None));
    let job = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(job.state, "completed");
    assert_eq!(
        amounts(&f, a).await,
        ("unknown".into(), Some(4_000_000), None, false)
    );
    // Failed by the provider: unknown (the price is not entirely free).
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let b = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), b)
        .await
        .unwrap();
    let mut failed = upstream_video(JobState::Failed, None);
    failed.error = Some(ErrorCode::parse("moderation_blocked"));
    fake.push_video(failed);
    let job = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(
        (job.state.as_str(), job.error_code.as_deref()),
        ("failed", Some("moderation_blocked"))
    );
    assert_eq!(amounts(&f, b).await.0, "unknown");
    assert_eq!(amounts(&f, b).await.1, Some(4_000_000));
    // The upstream create failing after its request was sent records a
    // failed attempt (hold kept) and ends its job row `create_unconfirmed`.
    let c = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), c).await.err(),
        Some(JobError::Inference(InferenceError::UpstreamUnavailable))
    );
    assert_eq!(amounts(&f, c).await.0, "unknown");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 3);
    let ended: (String, Option<String>) =
        sqlx::query_as("SELECT state,error_code FROM async_jobs WHERE execution_id=$1")
            .bind(c)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(ended, ("failed".into(), Some("create_unconfirmed".into())));
}

#[dbtest::test]
async fn jobs_are_private_to_their_workspace(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    let id = client_id("video_", job.id);
    // Another workspace's key: not found (not forbidden), no upstream call.
    assert_eq!(
        j.get_video(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.delete_video(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.video_content(&f.team, &id, VideoAsset::Video).await.err(),
        Some(JobError::NotFound)
    );
    assert!(
        j.list_videos(&f.team, None, 10, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        j.list_videos(&f.principal, None, 10, false)
            .await
            .unwrap()
            .len(),
        1
    );
    // A batch id with a video's uuid, or garbage, is not found either.
    assert_eq!(
        j.get_batch(&f.principal, &client_id("batch_", job.id))
            .await
            .err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.get_video(&f.principal, "video_../x").await.err(),
        Some(JobError::NotFound)
    );
    // Content needs a completed job; deleting needs a finished one.
    assert!(matches!(
        j.video_content(&f.principal, &id, VideoAsset::Video).await,
        Err(JobError::Conflict("video_not_ready", _))
    ));
    assert!(matches!(
        j.delete_video(&f.principal, &id).await,
        Err(JobError::Conflict("video_in_progress", _))
    ));
    assert_eq!(fake.calls(), ["create_video"]);
    // Disabling the connection stops client-initiated calls and polling.
    sqlx::query("UPDATE provider_connections SET enabled=false")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        j.get_video(&f.principal, &id).await.err(),
        Some(JobError::Inference(InferenceError::ModelUnavailable))
    );
}

#[dbtest::test]
async fn the_state_machine_is_forward_only_and_identity_immutable(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    // An observed regression is ignored.
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let row = j
        .refresh_video(job.clone(), super::store::Observer::Client)
        .await
        .unwrap();
    assert_eq!(row.state, "in_progress");
    // The database refuses invalid transitions and identity changes.
    for sql in [
        "UPDATE async_jobs SET state='queued'",
        "UPDATE async_jobs SET upstream_id='other'",
        "UPDATE async_jobs SET workspace_id=gen_random_uuid()",
        "DELETE FROM async_jobs",
    ] {
        assert!(
            sqlx::query(sql).execute(&f.store.pool).await.is_err(),
            "{sql}"
        );
    }
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE async_jobs SET state='completed'")
            .execute(&f.store.pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE async_jobs SET settled_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE async_jobs SET settled_at=now()+interval '1 second'")
            .execute(&f.store.pool)
            .await
            .is_err()
    );
}

pub(crate) async fn bearer(f: &Fixture, workspace: Uuid) -> String {
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'http',$4)").bind(key.id).bind(workspace).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    format!("Bearer {}", key.token)
}
async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    auth: &str,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", auth);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from(
                    body.map(|b| b.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[dbtest::test]
async fn http_video_routes_render_gateway_ids_and_scope_by_key(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake.clone())).unwrap();
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry,
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_engine(f.store.clone(), None, engine);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let theirs = bearer(&f, f.team.workspace_id).await;
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let (status, created) = send(
        &app,
        "POST",
        "/v1/videos",
        &mine,
        Some(json!({"model":"company/smart","prompt":"a calm lake","seconds":"8"})),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("video_") && !created.to_string().contains("video_up"));
    assert_eq!(
        (
            created["object"].as_str(),
            created["status"].as_str(),
            created["seconds"].as_str()
        ),
        (Some("video"), Some("queued"), Some("8"))
    );
    assert_eq!(created["model"], "company/smart");
    // Another workspace's key cannot see, list, download or delete it.
    for (method, path) in [
        ("GET", format!("/v1/videos/{id}")),
        ("GET", format!("/v1/videos/{id}/content")),
        ("DELETE", format!("/v1/videos/{id}")),
    ] {
        let (status, body) = send(&app, method, &path, &theirs, None).await;
        assert_eq!(status, 404, "{method} {path}: {body}");
        assert_eq!(body["error"]["code"], "not_found");
    }
    assert_eq!(
        send(&app, "GET", "/v1/videos", &theirs, None).await.1["data"],
        json!([])
    );
    let (_, list) = send(&app, "GET", "/v1/videos?limit=1", &mine, None).await;
    assert_eq!(
        (
            list["object"].as_str(),
            list["data"][0]["id"].as_str(),
            list["has_more"].as_bool()
        ),
        (Some("list"), Some(id.as_str()), Some(false))
    );
    // Not ready yet; bad queries are client errors; unsupported fields explicit.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/videos/{id}/content"),
        &mine,
        None,
    )
    .await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (400, Some("video_not_ready"))
    );
    assert_eq!(
        send(&app, "GET", "/v1/videos?limit=0", &mine, None).await.0,
        400
    );
    let (status, body) = send(&app, "POST", "/v1/videos", &mine, Some(json!({"model":"company/smart","prompt":"x","input_reference":{"image_url":"https://x"}}))).await;
    // An option the gateway does not take is refused by name.
    assert_eq!(
        (
            status.as_u16(),
            body["error"]["code"].as_str(),
            body["error"]["param"].as_str()
        ),
        (
            400,
            Some("unsupported_video_parameter"),
            Some("input_reference")
        )
    );
    // Batch routes answer explicitly too.
    // Without the file store, batches are refused explicitly.
    let (status, body) = send(&app, "POST", "/v1/batches", &mine, Some(json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (400, Some("batch_files_disabled"))
    );
    let (status, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &mine,
        Some(
            json!({"input_file_id":"file-x","endpoint":"/v1/audio/speech","completion_window":"24h"}),
        ),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(fake.calls(), ["create_video"]);
}

/// Active job slots of a workspace as admission counts them (0018).
async fn job_count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.workspace_id=$1 AND e.workload_kind IN('videos','batches') AND r.state='pending' AND r.lease_expires_at>now() AND NOT coalesce(j.state IN('completed','failed','cancelled','expired') OR j.cancel_requested_at IS NOT NULL,false)")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn video_jobs_are_exempt_from_per_minute_limits(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    // One request per minute and at once: both videos still start (job slots).
    f.policy("workspace_local_policies", Some(1), Some(1), Some(1), None)
        .await;
    for _ in 0..2 {
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .unwrap();
    }
    assert_eq!(
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .err(),
        Some(JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::Workspace
        )))
    );
    // Raising the workspace's job limit (platform override) admits a third.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,3)")
        .bind(f.principal.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    j.create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    // A key lineage limit still applies on top (there is no installation-wide limit).
    sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_jobs) SELECT workspace_id,governance_key_id,3 FROM api_keys WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_platform_policy_overrides SET concurrent_jobs=10")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .err(),
        Some(JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::ApiKey
        )))
    );
}

#[dbtest::test]
async fn concurrent_job_admissions_never_exceed_the_limit(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    for _ in 0..6 {
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
    }
    // Admission checks and inserts under the installation lock, so parallel
    // submissions cannot both take the last slot.
    let results = futures_util::future::join_all(
        (0..6).map(|_| j.create_video(f.principal, video_request(), Uuid::new_v4())),
    )
    .await;
    let admitted = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(admitted, 2, "{results:?}");
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| *e
        == JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::Workspace
        ))));
    assert_eq!(job_count(&f).await, 2);
}

#[dbtest::test]
async fn video_without_a_supported_provider_is_refused_before_admission(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    // The production OpenAI adapter: its Videos API was retired on 2026-09-24.
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(
            crate::providers::openai::OpenAiAdapter::new(Arc::new(
                crate::providers::secrets::EnvSecrets::new([]),
            ))
            .unwrap(),
        ))
        .unwrap();
    let j = Jobs::with(f.store.clone(), registry, JobLimits::default());
    let request_id = Uuid::new_v4();
    match j
        .create_video(f.principal, video_request(), request_id)
        .await
    {
        Err(JobError::Conflict(code, message)) => {
            assert_eq!(code, "unsupported_capability");
            assert_eq!(message, crate::jobs::video::NO_VIDEO_PROVIDER);
        }
        other => panic!("expected an explicit refusal, got {:?}", other.map(|_| ())),
    }
    // Nothing was admitted or fabricated.
    let rows: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM async_jobs)")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, (0, 0, 0));
}

/// Object visibility (#141) for video jobs: every caller type × both modes ×
/// list, retrieve, content and delete, for jobs of a member's key and of a
/// service-account key. Hidden jobs are `NotFound` (before any upstream call);
/// visible ones reach the job's own state checks.
#[dbtest::test]
async fn object_visibility_matrix_for_video_jobs(pool: PgPool) {
    use crate::visibility::test_support::{CALLERS, Owner, expected, may_modify, team};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    let t = team(&f).await;
    for mode in ["member", "workspace"] {
        t.mode(&f, mode).await;
        for owner in [Owner::Member, Owner::Service] {
            fake.push_video(upstream_video(JobState::Queued, Some(8)));
            let job = j
                .create_video(t.owner(owner).principal, video_request(), Uuid::new_v4())
                .await
                .unwrap();
            let id = client_id("video_", job.id);
            for who in CALLERS {
                let p = &t.get(who).principal;
                let (see, case) = (
                    expected(mode, owner, who),
                    format!("{mode}/{owner:?}/{who:?}"),
                );
                let listed = j.list_videos(p, None, 100, false).await.unwrap();
                assert_eq!(listed.iter().any(|r| r.id == job.id), see, "list {case}");
                let calls = fake.calls().len();
                let get = j.get_video(p, &id).await.err();
                let content = j.video_content(p, &id, VideoAsset::Video).await.err();
                let delete = j.delete_video(p, &id).await.err();
                for (op, err) in [("get", get), ("content", content), ("delete", delete)] {
                    assert_eq!(err != Some(JobError::NotFound), see, "{op} {case}: {err:?}");
                }
                assert_eq!(
                    matches!(delete, Some(JobError::Forbidden(..))),
                    see && !may_modify(owner, who),
                    "delete another member's video {case}: {delete:?}"
                );
                if !see {
                    assert_eq!(
                        fake.calls().len(),
                        calls,
                        "no upstream call when hidden {case}"
                    );
                    // A hidden job as a list cursor matches nothing (as a missing one).
                    let missing = client_id("video_", Uuid::nil());
                    assert_eq!(
                        j.list_videos(p, Some(&id), 100, false).await.unwrap().len(),
                        j.list_videos(p, Some(&missing), 100, false)
                            .await
                            .unwrap()
                            .len(),
                        "cursor {case}"
                    );
                }
            }
        }
    }
}

/// #497 on real PostgreSQL: the job poller's claim with every pooled
/// connection busy, or timing out on a lock, is counted once at the
/// `job_storage` stage with its cause; a claim that runs records nothing.
#[dbtest::test]
async fn a_failed_job_claim_names_its_cause(pool: PgPool) {
    use crate::db::{
        accounting_failure::tests::labels,
        diagnostics::{self, Handle},
    };
    use sqlx::Connection;
    // One connection, so holding it makes the claim's acquire time out. The
    // test asserts that timeout's cause, not its length; opening the
    // connection is set-up, bounded on its own (attempts of 3 s each, the
    // acquire timeout, within 60 s), so a slow machine cannot turn the first
    // connect into the timeout under test.
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET lock_timeout = '200ms'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with((*pool.connect_options()).clone());
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match one.acquire().await {
                Ok(_) => break,
                Err(sqlx::Error::PoolTimedOut) => {}
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .expect("the pool opens its connection");
    let store = Store::new(one);
    let jobs = Jobs::with(store, ProviderRegistry::default(), JobLimits::default());
    let poll = || async {
        let handle = Handle::new();
        let polled = diagnostics::scope(handle.clone(), jobs.poll_once()).await;
        (polled, labels(&handle.signals()))
    };
    let (polled, signals) = poll().await;
    assert_eq!(polled, Ok(0));
    assert!(signals.is_empty(), "nothing is recorded on success");

    let held = jobs.store.pool.acquire().await.unwrap();
    let (polled, signals) = poll().await;
    drop(held);
    assert_eq!(polled, Err(InferenceError::Storage));
    assert_eq!(signals, [("job_storage", "pool_timeout")]);

    let mut blocker = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql("BEGIN; LOCK TABLE async_jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut blocker)
        .await
        .unwrap();
    let (polled, signals) = poll().await;
    sqlx::query("ROLLBACK").execute(&mut blocker).await.unwrap();
    blocker.close().await.unwrap();
    assert_eq!(polled, Err(InferenceError::Storage));
    assert_eq!(signals, [("job_storage", "lock_timeout")]);
}

/// #497, #662: the job record is written before the provider call, so a
/// record that fails sends nothing: no provider call, no job row, and the
/// attempt settles as a known zero. Counted at `job_storage` (no hold is left
/// to reconciliation).
#[dbtest::test]
async fn a_job_record_failing_before_the_provider_call_sends_nothing(pool: PgPool) {
    use crate::db::{
        accounting_failure::tests::labels,
        diagnostics::{self, Handle},
    };
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    sqlx::raw_sql(
        r#"CREATE FUNCTION job_record_fault_497() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN RAISE EXCEPTION 'job record fault'; END $$;
           CREATE TRIGGER job_record_fault_497 BEFORE INSERT ON async_jobs
             FOR EACH ROW EXECUTE FUNCTION job_record_fault_497();"#,
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let handle = Handle::new();
    let created = diagnostics::scope(
        handle.clone(),
        j.create_video(f.principal, video_request(), request_id),
    )
    .await;
    assert!(matches!(
        created,
        Err(JobError::Inference(InferenceError::Storage))
    ));
    assert!(fake.calls().is_empty(), "nothing was sent");
    assert_eq!(video_row(&f, request_id).await, None);
    let (state, _, actual, _) = amounts(&f, request_id).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
    assert_eq!(
        labels(&handle.signals()),
        [("job_storage", "other_database")]
    );
}

/// #640, #662: the video job row and the extension of its reservation's
/// lease are one transaction, written before the provider call. When the
/// lease cannot be extended, no job row is left and the provider is never
/// called: the attempt settles as a known zero.
#[dbtest::test]
async fn a_video_lease_that_cannot_be_extended_leaves_no_job_row(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    refuse_lease_extensions(&f.store.pool).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let handle = crate::db::diagnostics::Handle::new();
    let created = crate::db::diagnostics::scope(
        handle.clone(),
        j.create_video(f.principal, video_request(), request_id),
    )
    .await;
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("job_storage", "other_database")]
    );
    assert!(matches!(
        created,
        Err(JobError::Inference(InferenceError::Storage))
    ));
    assert!(fake.calls().is_empty(), "nothing was sent");
    let jobs_left: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(jobs_left, 0);
    assert_eq!(amounts(&f, request_id).await.0, "settled");
    assert_eq!(amounts(&f, request_id).await.2, Some(0));
}

/// Reject later lease-extension UPDATEs; initial final-lease INSERTs remain valid.
pub(super) async fn refuse_lease_extensions(pool: &PgPool) {
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_lease_extension_640() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN
                  IF NEW.lease_expires_at > OLD.lease_expires_at THEN
                    RAISE EXCEPTION 'lease extension refused';
                  END IF;
                  RETURN NEW;
                END $$;
           CREATE TRIGGER refuse_lease_extension_640 BEFORE UPDATE ON governance_reservations
             FOR EACH ROW EXECUTE FUNCTION refuse_lease_extension_640();"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[dbtest::test]
async fn a_failed_video_record_commit_sends_nothing_and_leaves_no_job(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_job_commit_640() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'commit refused'; END $$; CREATE CONSTRAINT TRIGGER refuse_job_commit_640 AFTER INSERT ON async_jobs DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION refuse_job_commit_640()")
        .execute(&f.store.pool).await.unwrap();
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let handle = crate::db::diagnostics::Handle::new();
    let created = crate::db::diagnostics::scope(
        handle.clone(),
        j.create_video(f.principal, video_request(), request_id),
    )
    .await;
    assert!(matches!(
        created,
        Err(JobError::Inference(InferenceError::Storage))
    ));
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("job_storage", "other_database")]
    );
    assert!(fake.calls().is_empty(), "nothing was sent");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
    let (state, _, actual, _) = amounts(&f, request_id).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
}

#[dbtest::test]
async fn a_video_record_transaction_that_cannot_begin_is_a_storage_failure(pool: PgPool) {
    let f = fixture(pool).await;
    f.store.pool.close().await;
    let handle = crate::db::diagnostics::Handle::new();
    let result = crate::db::diagnostics::scope(
        handle.clone(),
        crate::governance::jobs::accept_job(
            &f.store,
            Uuid::new_v4(),
            UtcInstant::now(),
            async |_| Ok(()),
        ),
    )
    .await;
    assert_eq!(result, Err(InferenceError::Storage));
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("job_storage", "pool_closed")]
    );
}

/// A lock wait inside the job record (its lease extension waits on a
/// reservation row another transaction holds) that ends in `lock_timeout` is
/// a storage failure counted at the job stage.
#[dbtest::test]
async fn a_video_record_lock_timeout_is_a_storage_failure(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let start = f.start();
    crate::governance::admit(
        &f.store,
        &start,
        &crate::governance::tests::db::request(),
        30,
    )
    .await
    .unwrap();
    let limited = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET lock_timeout='200ms'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*f.store.pool.connect_options()).clone())
        .await
        .unwrap();
    let store = Store::new(limited);
    let mut blocker = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT 1 FROM governance_reservations WHERE execution_id=$1 FOR UPDATE")
        .bind(start.id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let handle = crate::db::diagnostics::Handle::new();
    let result = crate::db::diagnostics::scope(
        handle.clone(),
        crate::governance::jobs::accept_job(&store, start.id, UtcInstant::now(), async |_| Ok(())),
    )
    .await;
    blocker.rollback().await.unwrap();
    assert_eq!(result, Err(InferenceError::Storage));
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("job_storage", "lock_timeout")]
    );
    store.pool.close().await;
}

#[dbtest::test]
async fn a_video_reconciled_during_the_provider_call_is_recorded_and_finishes(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let hold = Arc::new(FinalizeHold::default());
    hold.arm();
    fake.0.lock().unwrap().video_hold = Some(hold.clone());
    let j = jobs(&f, &fake, false);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let create = j.create_video(f.principal, video_request(), request_id);
    let reconcile = async {
        hold.reached().await;
        sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1").bind(request_id).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            crate::governance::reconcile_expired(&f.store, 100)
                .await
                .unwrap(),
            1
        );
        hold.release();
    };
    let (job, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(create, reconcile)
    })
    .await
    .unwrap();
    let job = job.unwrap();
    assert_eq!(amounts(&f, request_id).await.0, "unknown");
    fake.push_video(upstream_video(JobState::Completed, Some(8)));
    let job = j
        .refresh_video(job, super::store::Observer::Client)
        .await
        .unwrap();
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    assert_eq!(amounts(&f, request_id).await.0, "unknown");
    assert_eq!(amounts(&f, request_id).await.2, None);
}

/// Video and 0016 batch observations: a poller writes only under its current
/// claim, a client read writes only while no poller holds the job, and
/// progress and counts never go back (#802). `state` stays compare-and-set.
#[dbtest::test]
async fn video_observations_are_fenced_and_never_go_back(pool: PgPool) {
    use super::store::{Observation, Observer, claim_due, observe, reload};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    let obs = |progress: u8, counts: u32| Observation {
        state: JobState::InProgress,
        upstream_status: "in_progress",
        progress: Some(progress),
        completed_at: None,
        expires_at: None,
        error: None,
        counts: Some(RequestCounts {
            total: 10,
            completed: counts,
            failed: 0,
        }),
    };
    let due = async || {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now(),runner_lease_until=CASE WHEN runner_lease_until IS NULL THEN NULL ELSE now()-interval '1 second' END WHERE id=$1")
            .bind(job.id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    };
    due().await;
    let (_, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    due().await;
    let (_, b) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    let current = reload(&f.store, job.id).await.unwrap();
    // B holds the job: B writes, A's (stale) claim and a client read do not.
    observe(&f.store, &current, &obs(50, 5), 30.0, Observer::Claim(&b))
        .await
        .unwrap();
    let row = reload(&f.store, job.id).await.unwrap();
    assert_eq!((row.progress, row.request_completed), (Some(50), Some(5)));
    for observer in [Observer::Claim(&a), Observer::Client] {
        observe(&f.store, &row, &obs(90, 9), 30.0, observer)
            .await
            .unwrap();
        let unchanged = reload(&f.store, job.id).await.unwrap();
        assert_eq!(
            (unchanged.progress, unchanged.request_completed),
            (Some(50), Some(5))
        );
    }
    // An older observation under the current claim never lowers them.
    observe(&f.store, &row, &obs(20, 2), 30.0, Observer::Claim(&b))
        .await
        .unwrap();
    let row = reload(&f.store, job.id).await.unwrap();
    assert_eq!((row.progress, row.request_completed), (Some(50), Some(5)));
    // Once no poller holds it, a client read writes, forward only.
    b.end_poll(&f.store, None).await.unwrap();
    observe(&f.store, &row, &obs(10, 1), 30.0, Observer::Client)
        .await
        .unwrap();
    observe(&f.store, &row, &obs(70, 7), 30.0, Observer::Client)
        .await
        .unwrap();
    let row = reload(&f.store, job.id).await.unwrap();
    assert_eq!((row.progress, row.request_completed), (Some(70), Some(7)));
}

/// A video observation that is refused (a stale claim) never settles the
/// job: the claim that recorded its terminal state settles it with its own
/// evidence (#802).
#[dbtest::test]
async fn a_refused_video_observation_never_settles(pool: PgPool) {
    use super::store::{Observation, Observer, claim_due, observe, reload};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    let due = async || {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now(),runner_lease_until=CASE WHEN runner_lease_until IS NULL THEN NULL ELSE now()-interval '1 second' END WHERE id=$1")
            .bind(job.id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    };
    due().await;
    let (_, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    due().await;
    let (_, b) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    // B records `completed` and has not settled yet.
    let current = reload(&f.store, job.id).await.unwrap();
    let completed = Observation {
        state: JobState::Completed,
        upstream_status: "completed",
        progress: Some(100),
        completed_at: None,
        expires_at: None,
        error: None,
        counts: None,
    };
    observe(&f.store, &current, &completed, 30.0, Observer::Claim(&b))
        .await
        .unwrap();
    // A's stale refresh reads `completed` without a duration: refused, it
    // must not settle the job with that evidence.
    fake.push_video(upstream_video(JobState::Completed, None));
    let seen = j
        .refresh_video(current.clone(), Observer::Claim(&a))
        .await
        .unwrap();
    assert!(
        seen.settled_at.is_none(),
        "a refused observation settles nothing"
    );
    // B settles with the known duration.
    fake.push_video(upstream_video(JobState::Completed, Some(8)));
    let settled = j
        .refresh_video(reload(&f.store, job.id).await.unwrap(), Observer::Claim(&b))
        .await
        .unwrap();
    assert!(settled.settled_at.is_some());
    let state: String =
        sqlx::query_scalar("SELECT state FROM governance_reservations WHERE execution_id=$1")
            .bind(job.execution_id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, "settled");
}

/// The job row of an execution: (state, upstream id, error code, settled).
async fn video_row(
    f: &Fixture,
    execution: Uuid,
) -> Option<(String, Option<String>, Option<String>, bool)> {
    sqlx::query_as("SELECT state,upstream_id,error_code,settled_at IS NOT NULL FROM async_jobs WHERE execution_id=$1")
        .bind(execution)
        .fetch_optional(&f.store.pool)
        .await
        .unwrap()
}
/// The ledger of an execution: (kind, amount), by kind.
async fn ledger(f: &Fixture, execution: Uuid) -> Vec<(String, Option<i64>)> {
    sqlx::query_as(
        "SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind",
    )
    .bind(execution)
    .fetch_all(&f.store.pool)
    .await
    .unwrap()
}

/// #662: the provider accepts the video, then the database refuses the write
/// that carries its id. The job stays tracked: a row exists, clients see it
/// queued (never running, no provider call), and once the create grace has
/// passed the poller ends it `create_interrupted` with its hold kept.
#[dbtest::test]
async fn a_record_failure_after_the_provider_accepts_leaves_a_tracked_job(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_video_id_662() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN RAISE EXCEPTION 'video id write refused'; END $$;
           CREATE TRIGGER refuse_video_id_662 BEFORE INSERT OR UPDATE ON async_jobs
             FOR EACH ROW WHEN (NEW.upstream_id IS NOT NULL) EXECUTE FUNCTION refuse_video_id_662();"#,
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let created = j
        .create_video(f.principal, video_request(), request_id)
        .await;
    assert!(matches!(
        created,
        Err(JobError::Inference(InferenceError::Storage))
    ));
    assert_eq!(fake.calls(), ["create_video"], "the provider accepted it");
    assert_eq!(
        video_row(&f, request_id).await,
        Some(("queued".into(), None, None, false)),
        "the job is tracked"
    );
    let (state, held, actual, _) = amounts(&f, request_id).await;
    assert_eq!(state, "pending");
    assert!(held.is_some_and(|h| h > 0));
    assert_eq!(actual, None);
    // Clients see it queued with no progress, and no provider call is made.
    let listed = j.list_videos(&f.principal, None, 10, false).await.unwrap();
    assert_eq!(listed.len(), 1);
    let id = client_id("video_", listed[0].id);
    let read = j.get_video(&f.principal, &id).await.unwrap();
    let shown = video::render(&read);
    assert_eq!(
        (shown["status"].as_str(), shown["progress"].as_i64()),
        (Some("queued"), Some(0))
    );
    assert!(shown["error"].is_null());
    assert!(matches!(
        j.video_content(&f.principal, &id, VideoAsset::Video).await,
        Err(JobError::Conflict("video_not_ready", _))
    ));
    assert!(matches!(
        j.delete_video(&f.principal, &id).await,
        Err(JobError::Conflict("video_in_progress", _))
    ));
    // Within the grace the poller leaves it alone.
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(video_row(&f, request_id).await.unwrap().0, "queued");
    // Past the grace: failed, settled as cancelled with unknown usage.
    let mut later = j.clone();
    later.video_create_grace = TimeDelta::seconds(0);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(later.poll_once().await.unwrap(), 1);
    assert_eq!(
        video_row(&f, request_id).await,
        Some((
            "failed".into(),
            None,
            Some("create_interrupted".into()),
            true
        ))
    );
    let (state, after, actual, _) = amounts(&f, request_id).await;
    assert_eq!((state.as_str(), after, actual), ("unknown", held, None));
    assert_eq!(
        ledger(&f, request_id).await,
        [("hold".into(), held), ("unknown".into(), Some(0))]
    );
    assert_eq!(
        fake.calls(),
        ["create_video"],
        "never polled with an empty id"
    );
    // A finished video without a provider id has no provider assets.
    let failed = video::render(&j.get_video(&f.principal, &id).await.unwrap());
    assert_eq!(failed["error"]["code"], "create_interrupted");
    j.delete_video(&f.principal, &id).await.unwrap();
    assert_eq!(fake.calls(), ["create_video"]);
}

/// A create that built no request: the row ends `create_failed`, settled,
/// and the attempt is a known zero (#814). A record that cannot be written
/// before the call sends nothing either: no row, a known zero.
#[dbtest::test]
async fn a_video_create_that_sent_nothing_settles_as_a_known_zero(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    fake.0
        .lock()
        .unwrap()
        .create_failures
        .push_back((true, InferenceError::Configuration));
    let a = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), a).await.err(),
        Some(JobError::Inference(InferenceError::Configuration))
    );
    assert_eq!(
        video_row(&f, a).await,
        Some(("failed".into(), None, Some("create_failed".into()), true))
    );
    let (state, held, actual, _) = amounts(&f, a).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
    assert_eq!(
        ledger(&f, a).await,
        [("hold".into(), held), ("settlement".into(), Some(0))]
    );
    // The record refused: the provider is never called.
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_video_record_662() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN RAISE EXCEPTION 'video record refused'; END $$;
           CREATE TRIGGER refuse_video_record_662 BEFORE INSERT ON async_jobs
             FOR EACH ROW EXECUTE FUNCTION refuse_video_record_662();"#,
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let b = Uuid::new_v4();
    let handle = crate::db::diagnostics::Handle::new();
    let created = crate::db::diagnostics::scope(
        handle.clone(),
        j.create_video(f.principal, video_request(), b),
    )
    .await;
    assert_eq!(
        created.err(),
        Some(JobError::Inference(InferenceError::Storage))
    );
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("job_storage", "other_database")]
    );
    assert_eq!(
        fake.calls(),
        ["create_video"],
        "only the first create was called"
    );
    assert_eq!(video_row(&f, b).await, None);
    assert_eq!(amounts(&f, b).await.0, "settled");
    assert_eq!(amounts(&f, b).await.2, Some(0));
}

/// A create that times out after its request was sent, on a provider without
/// idempotent create: one call only, the row ends `create_unconfirmed` and
/// the hold is kept (the provider may have the video).
#[dbtest::test]
async fn a_video_create_with_an_unknown_outcome_keeps_its_hold(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    fake.0
        .lock()
        .unwrap()
        .create_failures
        .push_back((false, InferenceError::Timeout));
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let id = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), id).await.err(),
        Some(JobError::Inference(InferenceError::Timeout))
    );
    assert_eq!(
        fake.calls(),
        ["create_video"],
        "no resend without idempotency"
    );
    assert_eq!(
        video_row(&f, id).await,
        Some((
            "failed".into(),
            None,
            Some("create_unconfirmed".into()),
            true
        ))
    );
    let (state, held, actual, _) = amounts(&f, id).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0));
}

/// Where the provider makes a create with the same key the same job, a
/// create whose outcome is unknown is resent once with that key, and the job
/// is tracked with the provider's id.
#[dbtest::test]
async fn an_idempotent_provider_gets_one_keyed_resend(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    {
        let mut script = fake.0.lock().unwrap();
        script.idempotent = true;
        script
            .create_failures
            .push_back((false, InferenceError::UpstreamUnavailable));
    }
    let j = jobs(&f, &fake, false);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let id = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), id)
        .await
        .unwrap();
    assert_eq!(fake.calls(), ["create_video", "create_video"]);
    let keys = fake.0.lock().unwrap().keys.clone();
    assert_eq!(
        keys,
        [
            format!("omg-video-{}", job.id),
            format!("omg-video-{}", job.id)
        ]
    );
    assert!(job.upstream_id.is_some());
    assert_eq!(job.state, "queued");
    assert_eq!(amounts(&f, id).await.0, "pending");
    // A second failure of the resend is not resent again.
    {
        let mut script = fake.0.lock().unwrap();
        script
            .create_failures
            .push_back((false, InferenceError::Timeout));
        script
            .create_failures
            .push_back((false, InferenceError::Timeout));
    }
    let other = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), other)
            .await
            .err(),
        Some(JobError::Inference(InferenceError::Timeout))
    );
    assert_eq!(fake.calls().len(), 4);
    assert_eq!(
        video_row(&f, other).await.unwrap().2.as_deref(),
        Some("create_unconfirmed")
    );
}

/// The process dies after the record, while the provider create runs (the
/// create future is never finished nor dropped). Lease reconciliation finds
/// nothing (the lease covers the job), and the first poll after the create
/// grace ends the job `create_interrupted` with its hold kept, without a
/// provider call.
#[dbtest::test]
async fn a_process_that_dies_during_the_create_leaves_a_job_that_ends_interrupted(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let hold = Arc::new(FinalizeHold::default());
    hold.arm();
    fake.0.lock().unwrap().video_hold = Some(hold.clone());
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let id = Uuid::new_v4();
    let mut create = Box::pin(j.create_video(f.principal, video_request(), id));
    tokio::select! {
        _ = &mut create => panic!("the create is held at the provider"),
        () = hold.reached() => {}
    }
    // Death: nothing after this point of the create ever runs.
    std::mem::forget(create);
    assert_eq!(
        video_row(&f, id).await,
        Some(("queued".into(), None, None, false))
    );
    assert_eq!(
        crate::governance::reconcile_expired(&f.store, 100)
            .await
            .unwrap(),
        0
    );
    let mut later = j.clone();
    later.video_create_grace = TimeDelta::seconds(0);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(later.poll_once().await.unwrap(), 1);
    assert_eq!(
        video_row(&f, id).await,
        Some((
            "failed".into(),
            None,
            Some("create_interrupted".into()),
            true
        ))
    );
    let (state, held, actual, _) = amounts(&f, id).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0));
    assert_eq!(fake.calls(), ["create_video"]);
}

/// The failed create's attempt is recorded before its row ends, and the row
/// is marked settled only when that record succeeded: here it fails (the
/// ledger refuses it), the row ends `create_unconfirmed` unsettled, and a
/// later read settles it, conservatively (unknown, hold kept).
#[dbtest::test]
async fn a_failed_create_whose_record_fails_is_settled_by_a_later_read(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    fake.0
        .lock()
        .unwrap()
        .create_failures
        .push_back((false, InferenceError::Timeout));
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_ledger_662() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN IF NEW.kind<>'hold' THEN RAISE EXCEPTION 'ledger refused'; END IF; RETURN NEW; END $$;
           CREATE TRIGGER refuse_ledger_662 BEFORE INSERT ON monetary_ledger
             FOR EACH ROW EXECUTE FUNCTION refuse_ledger_662();"#,
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let id = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), id).await.err(),
        Some(JobError::Inference(InferenceError::Timeout))
    );
    assert_eq!(
        video_row(&f, id).await,
        Some((
            "failed".into(),
            None,
            Some("create_unconfirmed".into()),
            false
        ))
    );
    assert_eq!(amounts(&f, id).await.0, "pending");
    sqlx::raw_sql("DROP TRIGGER refuse_ledger_662 ON monetary_ledger")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let job_id: Uuid = sqlx::query_scalar("SELECT id FROM async_jobs WHERE execution_id=$1")
        .bind(id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    j.get_video(&f.principal, &client_id("video_", job_id))
        .await
        .unwrap();
    assert!(video_row(&f, id).await.unwrap().3, "settled");
    let (state, held, actual, _) = amounts(&f, id).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0));
    assert_eq!(fake.calls(), ["create_video"]);
}

/// Once the first outcome was unknown, a resend that the provider refuses
/// does not turn the job into "answered without a job": it stays
/// `create_unconfirmed` and its hold is kept, while the client sees the
/// resend's answer.
#[dbtest::test]
async fn a_refused_resend_after_an_unknown_outcome_stays_unconfirmed(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    {
        let mut script = fake.0.lock().unwrap();
        script.idempotent = true;
        script
            .create_failures
            .push_back((false, InferenceError::Timeout));
        script
            .create_failures
            .push_back((false, InferenceError::UpstreamRejected));
    }
    let j = jobs(&f, &fake, false);
    let id = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), id).await.err(),
        Some(JobError::Inference(InferenceError::UpstreamRejected))
    );
    assert_eq!(fake.calls(), ["create_video", "create_video"]);
    assert_eq!(
        video_row(&f, id).await,
        Some((
            "failed".into(),
            None,
            Some("create_unconfirmed".into()),
            true
        ))
    );
    assert_eq!(amounts(&f, id).await.0, "unknown");
}

/// An adapter that declares idempotent create but does not send the key
/// (the default keyed create) is refused before anything is sent: a resend
/// without the key would be a second job.
#[tokio::test]
async fn the_default_keyed_create_refuses_an_idempotent_declaration() {
    struct Declares;
    #[async_trait]
    impl ProviderAdapter for Declares {
        fn id(&self) -> &'static str {
            "openai"
        }
        fn capabilities(&self) -> crate::inference::types::Capabilities {
            crate::inference::types::Capabilities {
                image_input: false,
                text_chat: false,
                streaming: false,
                tools: false,
            }
        }
        fn idempotent_video_create(&self, _: &Deployment) -> bool {
            true
        }
        async fn create_video(
            &self,
            _: &Deployment,
            _: VideoRequest,
        ) -> Result<UpstreamVideo, InferenceError> {
            panic!("never called without the key");
        }
        async fn execute(
            &self,
            _: &Deployment,
            _: crate::inference::types::ChatRequest,
        ) -> Result<crate::inference::types::ProviderOutput, InferenceError> {
            Err(InferenceError::Unsupported)
        }
    }
    let target = Deployment {
        image_input: false,
        id: Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: "m".into(),
        credential_ref: "none".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["videos".into()],
    };
    let (created, nothing_sent) = crate::inference::dispatch::observe(Declares.create_video_keyed(
        &target,
        video_request(),
        &VideoCreateKey::for_job(Uuid::new_v4()),
    ))
    .await;
    assert_eq!(created.err(), Some(InferenceError::Configuration));
    assert!(nothing_sent);
}

/// #802 with #662: a video held at its provider create, recorded without a
/// provider id. The create future is returned so the test decides whether it
/// answers or dies.
async fn held_create<'a>(
    f: &'a Fixture,
    j: &'a Jobs,
    fake: &Fake,
    answer: UpstreamVideo,
) -> (
    Uuid,
    Arc<FinalizeHold>,
    std::pin::Pin<Box<impl std::future::Future<Output = JobResult<JobRow>> + 'a>>,
) {
    let hold = Arc::new(FinalizeHold::default());
    hold.arm();
    fake.0.lock().unwrap().video_hold = Some(hold.clone());
    fake.push_video(answer);
    let id = Uuid::new_v4();
    let mut create = Box::pin(j.create_video(f.principal, video_request(), id));
    tokio::select! {
        _ = &mut create => panic!("the create is held at the provider"),
        () = hold.reached() => {}
    }
    (id, hold, create)
}
async fn row_of(f: &Fixture, execution: Uuid) -> JobRow {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM async_jobs WHERE execution_id=$1")
        .bind(execution)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    super::store::reload(&f.store, id).await.unwrap()
}
async fn make_due(f: &Fixture, job: Uuid) {
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(job)
        .execute(&f.store.pool)
        .await
        .unwrap();
}

/// The creator's first observation is written with no poller able to hold
/// the row: a recorded video is first due at the end of its create grace, so
/// no claim takes it while the create runs; the create's answer is recorded.
#[dbtest::test]
async fn no_poller_holds_a_video_while_its_create_runs(pool: PgPool) {
    use super::store::claim_due;
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let (execution, hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::InProgress, Some(8))).await;
    let row = row_of(&f, execution).await;
    let (due_after_grace,): (bool,) = sqlx::query_as(
        // `submit_started_at` and `next_poll_at` are two clock readings of
        // one insert: allow the microseconds between them.
        "SELECT next_poll_at>=submit_started_at+make_interval(secs=>$2)-interval '1 second' FROM async_jobs WHERE id=$1",
    )
    .bind(row.id)
    .bind(j.video_create_grace().whole_seconds() as f64)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert!(due_after_grace, "first due at the end of the grace");
    assert!(
        claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
            .await
            .unwrap()
            .is_none(),
        "no claim inside the grace"
    );
    hold.release();
    let created = create.await.unwrap();
    assert_eq!(
        created.state, "in_progress",
        "the creator's observation is recorded"
    );
    assert!(created.upstream_id.is_some());
}

/// A create that answers after the grace while a poller holds the row: the
/// creator records the provider id (the row is still a queued video without
/// one) and its first observation, which ignores claims: it is the only copy
/// of the create's answer, so it records and settles with the known
/// duration. The claim read the row before the id landed: its end of the
/// row "without an id" is refused, and so is the creator's own.
#[dbtest::test]
async fn a_late_create_and_a_live_claim_never_both_write(pool: PgPool) {
    use super::store::{Observer, claim_due};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (execution, hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::Completed, Some(8))).await;
    let row = row_of(&f, execution).await;
    make_due(&f, row.id).await;
    let (snapshot, fence) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.upstream_id.is_none());
    hold.release();
    let created = create.await.unwrap();
    assert!(created.upstream_id.is_some(), "the creator recorded the id");
    assert_eq!(created.state, "completed", "its observation is recorded");
    assert!(created.settled_at.is_some(), "and settles");
    let settled = amounts(&f, execution).await;
    assert_eq!(settled.0, "settled");
    assert!(settled.2.is_some_and(|a| a > 0), "with the known duration");
    // The claim read the row before the id landed: it changes nothing.
    let seen = j
        .refresh_video(snapshot, Observer::Claim(&fence))
        .await
        .unwrap();
    assert_eq!(seen.state, "completed");
    assert_eq!(seen.error_code, None);
    // Nor can the creator end a row whose id is recorded.
    assert!(
        super::store::fail_unrecorded_video(
            &f.store,
            row.id,
            "create_failed",
            None,
            Observer::Creator
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(amounts(&f, execution).await, settled);
}

/// The poller ends the row first (after the grace): the create's late answer
/// cannot record its id on the ended row, and the row keeps its
/// `create_interrupted` end and its retained hold.
#[dbtest::test]
async fn a_create_answering_after_the_poller_ended_the_row_changes_nothing(pool: PgPool) {
    use super::store::{Observer, claim_due};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (execution, hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::InProgress, Some(8))).await;
    let row = row_of(&f, execution).await;
    make_due(&f, row.id).await;
    let (snapshot, fence) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    let ended = j
        .refresh_video(snapshot, Observer::Claim(&fence))
        .await
        .unwrap();
    assert_eq!(
        (ended.state.as_str(), ended.error_code.as_deref()),
        ("failed", Some("create_interrupted"))
    );
    assert!(ended.settled_at.is_some());
    let settled = amounts(&f, execution).await;
    assert_eq!(settled.0, "unknown");
    hold.release();
    assert!(create.await.is_err(), "the id arrived after the job ended");
    let after = row_of(&f, execution).await;
    assert_eq!(after.upstream_id, None);
    assert_eq!(after.error_code.as_deref(), Some("create_interrupted"));
    assert_eq!(amounts(&f, execution).await, settled);
}

/// A row without a provider id past its grace: a client read while a poller
/// holds the row, and a stale claim, end and settle nothing; the current
/// claim ends it `create_interrupted`.
#[dbtest::test]
async fn only_the_current_writer_ends_a_row_without_an_id(pool: PgPool) {
    use super::store::{Observer, claim_due};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (execution, _hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::InProgress, Some(8))).await;
    std::mem::forget(create);
    let row = row_of(&f, execution).await;
    let lapse = async || {
        sqlx::query("UPDATE async_jobs SET runner_lease_until=CASE WHEN runner_lease_until IS NULL THEN NULL ELSE now()-interval '1 second' END,next_poll_at=now() WHERE id=$1")
            .bind(row.id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    };
    lapse().await;
    let (stale_snapshot, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    lapse().await;
    let (snapshot, b) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    let id = client_id("video_", row.id);
    let read = j.get_video(&f.principal, &id).await.unwrap();
    assert_eq!(
        (read.state.as_str(), read.settled_at),
        ("queued", None),
        "a read during a claim"
    );
    let stale = j
        .refresh_video(stale_snapshot, Observer::Claim(&a))
        .await
        .unwrap();
    assert_eq!(
        (stale.state.as_str(), stale.settled_at),
        ("queued", None),
        "a stale claim"
    );
    assert_eq!(amounts(&f, execution).await.0, "pending");
    let ended = j
        .refresh_video(snapshot, Observer::Claim(&b))
        .await
        .unwrap();
    assert_eq!(ended.error_code.as_deref(), Some("create_interrupted"));
    assert!(ended.settled_at.is_some());
    assert_eq!(amounts(&f, execution).await.0, "unknown");
}

/// A terminal row without an id whose settled mark was lost (its create
/// failed): settling it is not an observation, so it is made only by the
/// writer that may write the row now (the current claim, or a read while no
/// claim is live), never by a stale claim or a read during a live one.
#[dbtest::test]
async fn a_lost_settled_mark_is_restored_only_by_the_current_writer(pool: PgPool) {
    use super::store::{Observer, claim_due, fail_unrecorded_video};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let (execution, _hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::InProgress, Some(8))).await;
    std::mem::forget(create);
    let row = row_of(&f, execution).await;
    // The create failed and its settled mark was lost.
    let failed = fail_unrecorded_video(
        &f.store,
        row.id,
        "create_unconfirmed",
        None,
        Observer::Creator,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(failed.settled_at.is_none());
    let lapse = async || {
        sqlx::query("UPDATE async_jobs SET runner_lease_until=CASE WHEN runner_lease_until IS NULL THEN NULL ELSE now()-interval '1 second' END,next_poll_at=now() WHERE id=$1")
            .bind(row.id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    };
    lapse().await;
    let (stale_snapshot, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    lapse().await;
    let (snapshot, b) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    let id = client_id("video_", row.id);
    assert!(
        j.get_video(&f.principal, &id)
            .await
            .unwrap()
            .settled_at
            .is_none()
    );
    assert!(
        j.refresh_video(stale_snapshot, Observer::Claim(&a))
            .await
            .unwrap()
            .settled_at
            .is_none()
    );
    assert_eq!(amounts(&f, execution).await.0, "pending");
    let settled = j
        .refresh_video(snapshot, Observer::Claim(&b))
        .await
        .unwrap();
    assert!(settled.settled_at.is_some());
    let (state, held, actual, _) = amounts(&f, execution).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0), "the hold is kept");
}

/// A row without a provider id past its grace (the create's process died).
async fn orphaned_video(f: &Fixture, j: &Jobs, fake: &Fake) -> (Uuid, JobRow) {
    let (execution, _hold, create) =
        held_create(f, j, fake, upstream_video(JobState::InProgress, Some(8))).await;
    std::mem::forget(create);
    let row = row_of(f, execution).await;
    make_due(f, row.id).await;
    (execution, row)
}

/// A settlement is made under the writer's hold on the job row: while a claim
/// holds the row (`hold_writer`), no other poller can take the job, even once
/// the lease lapsed, so the claim that checked it may write is the one that
/// settles. After the hold ends the row is taken, and the old claim's hold is
/// refused.
#[dbtest::test]
async fn a_writer_hold_keeps_a_takeover_out_until_it_ends(pool: PgPool) {
    use super::store::{Observer, claim_due, hold_writer};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (_, row) = orphaned_video(&f, &j, &fake).await;
    let (_, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    assert!(
        hold_writer(&mut tx, row.id, Observer::Claim(&a))
            .await
            .unwrap()
    );
    sqlx::query("UPDATE async_jobs SET runner_lease_until=now()-interval '1 second',next_poll_at=now() WHERE id=$1")
        .bind(row.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
            .await
            .unwrap()
            .is_none(),
        "no takeover while the writer holds the row"
    );
    tx.commit().await.unwrap();
    let (_, b) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    assert!(b.epoch > a.epoch);
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    assert!(
        !hold_writer(&mut tx, row.id, Observer::Claim(&a))
            .await
            .unwrap()
    );
    assert!(
        !hold_writer(&mut tx, row.id, Observer::Client)
            .await
            .unwrap()
    );
    assert!(
        hold_writer(&mut tx, row.id, Observer::Claim(&b))
            .await
            .unwrap()
    );
}

/// A claim whose lease lapsed with no takeover writes nothing: it is not the
/// current writer, and a client read, which may write while no claim is
/// live, ends and settles the row. The two are never both writers.
#[dbtest::test]
async fn an_expired_claim_writes_nothing_and_a_read_then_may(pool: PgPool) {
    use super::store::{Observer, claim_due};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (execution, row) = orphaned_video(&f, &j, &fake).await;
    let (snapshot, a) = claim_due(&f.store, Uuid::new_v4(), 30.0, 60.0)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE async_jobs SET runner_lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(row.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let stale = j
        .refresh_video(snapshot, Observer::Claim(&a))
        .await
        .unwrap();
    assert_eq!((stale.state.as_str(), stale.settled_at), ("queued", None));
    assert_eq!(amounts(&f, execution).await.0, "pending");
    let read = j
        .get_video(&f.principal, &client_id("video_", row.id))
        .await
        .unwrap();
    assert_eq!(read.error_code.as_deref(), Some("create_interrupted"));
    assert!(read.settled_at.is_some());
    assert_eq!(amounts(&f, execution).await.0, "unknown");
}

/// A row without a provider id is never polled upstream, so its poll
/// deadline does not end its claims: one past its deadline is still claimed,
/// ended `create_interrupted` and settled with its hold kept, instead of
/// staying queued and unsettled for good.
#[dbtest::test]
async fn a_video_without_an_id_past_its_poll_deadline_is_still_ended(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    j.video_create_grace = TimeDelta::seconds(0);
    let (execution, row) = orphaned_video(&f, &j, &fake).await;
    sqlx::query("UPDATE async_jobs SET poll_deadline_at=now()-interval '1 second' WHERE id=$1")
        .bind(row.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    let ended = row_of(&f, execution).await;
    assert_eq!(ended.error_code.as_deref(), Some("create_interrupted"));
    assert!(ended.settled_at.is_some());
    let (state, held, actual, _) = amounts(&f, execution).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0), "the hold is kept");
}

/// An observation whose state the state machine did not take is not the
/// row's evidence: a `failed` read of a video recorded `completed` (its
/// settlement not yet made) settles nothing; the next `completed` read
/// settles with its duration.
#[dbtest::test]
async fn an_observation_the_state_machine_did_not_take_settles_nothing(pool: PgPool) {
    use super::store::Observer;
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::InProgress, None));
    let created = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    let execution = created.execution_id;
    sqlx::query("UPDATE async_jobs SET state='completed',completed_at=now() WHERE id=$1")
        .bind(created.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let completed = super::store::reload(&f.store, created.id).await.unwrap();
    fake.push_video(upstream_video(JobState::Failed, None));
    let read = j
        .refresh_video(completed.clone(), Observer::Client)
        .await
        .unwrap();
    assert_eq!((read.state.as_str(), read.settled_at), ("completed", None));
    assert_eq!(amounts(&f, execution).await.0, "pending");
    fake.push_video(upstream_video(JobState::Completed, Some(8)));
    let settled = j.refresh_video(read, Observer::Client).await.unwrap();
    assert!(settled.settled_at.is_some());
    let (state, _, actual, _) = amounts(&f, execution).await;
    assert_eq!(state, "settled");
    assert!(actual.is_some_and(|a| a > 0));
}

/// A writer's hold keeps a pool connection while its settlement takes others
/// from the same pool: more concurrent settlements than connections must
/// still all return, and the job settles once. Here, client reads of one
/// terminal, unsettled row without an id on a two-connection pool.
#[dbtest::test]
async fn concurrent_settlements_never_exhaust_the_pool(pool: PgPool) {
    use super::store::{Observer, fail_unrecorded_video};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut j = jobs(&f, &fake, true);
    let (execution, _hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::InProgress, Some(8))).await;
    std::mem::forget(create);
    let row = row_of(&f, execution).await;
    // The create failed and its settled mark was lost.
    fail_unrecorded_video(
        &f.store,
        row.id,
        "create_unconfirmed",
        None,
        Observer::Creator,
    )
    .await
    .unwrap()
    .unwrap();
    j.store.pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(crate::config::ACQUIRE_TIMEOUT)
        .connect_with((*f.store.pool.connect_options()).clone())
        .await
        .unwrap();
    let id = client_id("video_", row.id);
    let reads = futures::future::join_all((0..6).map(|_| j.get_video(&f.principal, &id)));
    let reads = tokio::time::timeout(Duration::from_secs(30), reads)
        .await
        .expect("every read returns");
    for read in reads {
        assert!(read.unwrap().settled_at.is_some());
    }
    let (state, held, actual, _) = amounts(&f, execution).await;
    assert_eq!((state.as_str(), actual), ("unknown", None));
    assert!(held.is_some_and(|h| h > 0), "the hold is kept");
    assert_eq!(
        ledger(&f, execution).await,
        [("hold".into(), held), ("unknown".into(), Some(0))],
        "settled once"
    );
}

/// Recording a provider id takes the advisory lock of (upstream account,
/// id): while another transaction holds it, the record waits, so two
/// creates answered with one id are checked one after the other.
#[dbtest::test]
async fn recording_a_provider_id_waits_for_its_account_lock(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let (execution, _hold, create) =
        held_create(&f, &j, &fake, upstream_video(JobState::Queued, Some(8))).await;
    std::mem::forget(create);
    let row = row_of(&f, execution).await;
    let account: String = sqlx::query_scalar("SELECT p.provider||':'||coalesce(p.endpoint,'')||':'||p.credential_ref FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1")
        .bind(row.deployment_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let mut holder = f.store.pool.acquire().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_lock(hashtextextended('video_upstream:'||$1||':'||'video_locked',0))",
    )
    .bind(&account)
    .execute(&mut *holder)
    .await
    .unwrap();
    let upstream = UpstreamId::parse("video_locked").unwrap();
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        super::store::record_video_upstream(&f.store, row.id, &upstream),
    )
    .await;
    assert!(waited.is_err(), "the record waits for the lock");
    sqlx::query("SELECT pg_advisory_unlock_all()")
        .execute(&mut *holder)
        .await
        .unwrap();
    let recorded = super::store::record_video_upstream(&f.store, row.id, &upstream)
        .await
        .unwrap()
        .expect("recorded once the lock is free");
    assert_eq!(recorded.upstream_id.as_deref(), Some("video_locked"));
}

/// A reported size for a request that named none is valued by the price
/// pinned at admission, never a later one.
#[dbtest::test]
async fn a_reported_size_is_valued_by_the_admitted_price(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let no_size = VideoRequest {
        size: None,
        ..video_request()
    };
    let job = j
        .create_video(f.principal, no_size, Uuid::new_v4())
        .await
        .unwrap();
    // A later price without size-specific lines (one rate for all) would
    // value any size; the admitted one names 1792x1024 itself.
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.push(line(Meter::OutputVideoSecondsMs, "100000", 1000));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
    use crate::governance::{ReportedSizeRate, pinned_video_size_rate};
    assert_eq!(
        pinned_video_size_rate(&f.store, job.execution_id, "1792x1024")
            .await
            .unwrap(),
        ReportedSizeRate::Named
    );
    assert_eq!(
        pinned_video_size_rate(&f.store, job.execution_id, "640x360")
            .await
            .unwrap(),
        ReportedSizeRate::Unpriced,
        "not the later flat price"
    );
}
