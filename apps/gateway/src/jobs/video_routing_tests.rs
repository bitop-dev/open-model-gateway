//! #52: which route a video request goes to, through the running gateway
//! against the mock contract server (`video_contract_tests`): routes with
//! different options, an unpriced route ahead of a priced one, a route
//! without options ahead of one that takes the request, a drained route, a
//! refusal once the key may no longer use the model, and creates the server
//! refuses.
use super::db_tests::{line, na};
use super::video_contract_tests::{Contract, contract, field};
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
use std::sync::Arc;

struct Rig {
    pool: PgPool,
    f: Fixture,
    mock: Arc<Contract>,
    gateway: String,
    connection: Uuid,
    auth: String,
    key: Uuid,
    _servers: (Server, Server),
}

/// The price lines of a route: tokens not applicable, no request fee, and
/// `video` as its video-seconds lines.
fn lines(video: Vec<Value>) -> Value {
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.extend(video);
    json!(lines)
}
fn flat() -> Value {
    lines(vec![line(Meter::OutputVideoSecondsMs, "100000", 1000)])
}

impl Rig {
    async fn new(pool: PgPool) -> Self {
        let f = fixture(pool.clone()).await;
        // Room for every job these tests start.
        sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('personal',20) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs")
            .execute(&pool).await.unwrap();
        let mock = Arc::new(Contract::default());
        let (upstream, upstream_server) = serve(contract(mock.clone())).await;
        let base = format!("{upstream}/v1");
        let connection = Uuid::new_v4();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Clip server','openai_compatible','none',$2,true)")
            .bind(connection).bind(&base).execute(&pool).await.unwrap();
        let mut registry = ProviderRegistry::default();
        for adapter in crate::providers::local::adapters(
            Arc::new(EnvSecrets::new([])),
            ApprovedEndpoints::for_test(&base),
        ) {
            registry.register(adapter).unwrap();
        }
        let engine =
            Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
        let (gateway, gateway_server) = serve(crate::http::router_with_engine(
            f.store.clone(),
            None,
            engine,
        ))
        .await;
        let key = crate::auth::NewApiKey::generate().unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'http',$4)")
            .bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice())
            .execute(&pool).await.unwrap();
        Self {
            pool,
            mock,
            gateway,
            connection,
            auth: format!("Bearer {}", key.token),
            key: key.id,
            f,
            _servers: (upstream_server, gateway_server),
        }
    }
    /// A video model offered to the caller's workspace.
    async fn model(&self, name: &str) -> Uuid {
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
        model
    }
    /// A route of `model` to the server's `upstream` model, with these video
    /// options (none: `seconds` empty) and price, at routing `priority`.
    async fn route(
        &self,
        model: Uuid,
        upstream: &str,
        seconds: &[i32],
        sizes: Option<&[&str]>,
        price: Option<Value>,
        priority: i32,
    ) -> Uuid {
        let route = Uuid::new_v4();
        let seconds = (!seconds.is_empty()).then(|| seconds.to_vec());
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds,video_sizes) VALUES($1,$2,$3,$4,true,$5,$6)")
            .bind(route).bind(model).bind(self.connection).bind(upstream).bind(seconds).bind(sizes)
            .execute(&self.pool).await.unwrap();
        if let Some(price) = price {
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
                .bind(Uuid::new_v4()).bind(route).bind(price).execute(&self.pool).await.unwrap();
        }
        sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,$2)")
            .bind(route)
            .bind(priority)
            .execute(&self.pool)
            .await
            .unwrap();
        route
    }
    async fn create(&self, body: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/v1/videos", self.gateway))
            .header("authorization", &self.auth)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }
    /// The server model and length of the last create the server received.
    fn sent(&self) -> (Option<String>, Option<String>) {
        let forms = self.mock.forms.lock().unwrap();
        let form = forms.last().expect("a create was sent");
        (field(form, "model"), field(form, "seconds"))
    }
    fn creates(&self) -> usize {
        self.mock.forms.lock().unwrap().len()
    }
}

/// Among routes with different options, the request goes to one that takes
/// it, with that route's first length when it names none; an unpriced route
/// ahead of a priced one, a route without options and a drained route are
/// passed over; a combination no route takes is refused naming neither.
#[dbtest::test]
async fn a_video_goes_to_a_route_that_takes_and_prices_it(pool: PgPool) {
    let r = Rig::new(pool).await;
    let m = r.model("company/clips").await;
    let (a, b) = ("1280x720", "720x1280");
    r.route(m, "short", &[4, 8], Some(&[a]), Some(flat()), 0)
        .await;
    r.route(m, "long", &[12], Some(&[b]), Some(flat()), 1).await;
    let create = |extra: Value| {
        let mut body = json!({"model":"company/clips","prompt":"a lake"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        r.create(body)
    };
    let (status, body) = create(json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        r.sent(),
        (Some("short".into()), Some("4".into())),
        "the first route, at its first length"
    );
    assert_eq!(body["seconds"], "4", "the admitted length");
    let (status, body) = create(json!({"seconds":"12"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(r.sent(), (Some("long".into()), Some("12".into())));
    let (status, body) = create(json!({"size":b})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        r.sent(),
        (Some("long".into()), Some("12".into())),
        "the route of that size, at its own first length"
    );
    // Each taken by one route, never together.
    let sent = r.creates();
    let (status, body) = create(json!({"seconds":"4","size":b})).await;
    assert_eq!(
        (
            status,
            body["error"]["code"].clone(),
            body["error"]["param"].clone()
        ),
        (
            StatusCode::BAD_REQUEST,
            json!("unsupported_video_parameter"),
            Value::Null
        ),
        "{body}"
    );
    let (status, body) = create(json!({"seconds":"9"})).await;
    assert_eq!(
        (status, body["error"]["param"].clone()),
        (StatusCode::BAD_REQUEST, json!("seconds")),
        "{body}"
    );
    assert_eq!(r.creates(), sent, "nothing sent for a refusal");

    // An unpriced route ahead of a priced one; a route without options
    // ahead of both; a drained route ahead of all three.
    let m = r.model("company/priced").await;
    r.route(m, "no-options", &[], None, Some(flat()), 0).await;
    let drained = r.route(m, "drained", &[4], None, Some(flat()), 1).await;
    r.route(m, "unpriced", &[4], None, Some(lines(vec![])), 2)
        .await;
    r.route(m, "priced", &[4], None, Some(flat()), 3).await;
    sqlx::query("UPDATE deployments SET drained_at=now(),drain_reason='maintenance',drained_by=$2 WHERE id=$1")
        .bind(drained)
        .bind(r.f.owner)
        .execute(&r.pool)
        .await
        .unwrap();
    let (status, body) = r
        .create(json!({"model":"company/priced","prompt":"a lake"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(r.sent().0.as_deref(), Some("priced"));
    // Only unpriced routes take it: refused before anything is sent.
    let m = r.model("company/unpriced").await;
    r.route(m, "unpriced", &[4], None, Some(lines(vec![])), 0)
        .await;
    r.route(
        m,
        "sized-only",
        &[4],
        None,
        Some(lines(vec![{
            let mut l = line(Meter::OutputVideoSecondsMs, "100000", 1000);
            l["variant"] = json!(a);
            l
        }])),
        1,
    )
    .await;
    let sent = r.creates();
    let (status, body) = r
        .create(json!({"model":"company/unpriced","prompt":"a lake"}))
        .await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("video_price_missing")),
        "{body}"
    );
    assert_eq!(r.creates(), sent);
}

/// Once the key may no longer use the model, a request is answered as for a
/// model it cannot see (`404`), never with which length its routes take.
#[dbtest::test]
async fn a_refusal_never_describes_a_model_the_key_may_not_use(pool: PgPool) {
    let r = Rig::new(pool).await;
    let m = r.model("company/clips").await;
    r.route(m, "short", &[4], None, Some(flat()), 0).await;
    let body = json!({"model":"company/clips","prompt":"a lake","seconds":"9"});
    let (status, refused) = r.create(body.clone()).await;
    assert_eq!(
        (status, refused["error"]["param"].clone()),
        (StatusCode::BAD_REQUEST, json!("seconds")),
        "{refused}"
    );
    // A restriction without a selection: the key may use no model.
    sqlx::query("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)")
        .bind(r.f.principal.workspace_id)
        .bind(r.key)
        .execute(&r.pool)
        .await
        .unwrap();
    let (status, refused) = r.create(body).await;
    assert_eq!(
        (status, refused["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("model_not_found")),
        "{refused}"
    );
}

/// A create the server refuses (`404`, `409`, `429`) ends the job
/// `create_failed` and the caller gets the server's refusal class
/// (`upstream_rejected`, or `429` when it is busy). The request was sent, so
/// its hold is kept as unknown cost, as for every sent attempt that fails.
#[dbtest::test]
async fn a_create_the_server_refuses_ends_the_job_with_its_hold_kept(pool: PgPool) {
    let r = Rig::new(pool).await;
    let m = r.model("company/clips").await;
    r.route(m, "short", &[4], None, Some(flat()), 0).await;
    for (upstream, answered) in [
        (404, StatusCode::BAD_REQUEST),
        (409, StatusCode::BAD_REQUEST),
        (429, StatusCode::TOO_MANY_REQUESTS),
    ] {
        *r.mock.create_status.lock().unwrap() = Some(upstream);
        let (status, body) = r
            .create(json!({"model":"company/clips","prompt":"a lake"}))
            .await;
        assert_eq!(status, answered, "{upstream}: {body}");
        let (state, code, held, actual): (String, String, String, Option<i64>) = sqlx::query_as("SELECT j.state,j.error_code,r.state,r.actual_microusd FROM async_jobs j JOIN governance_reservations r ON r.execution_id=j.execution_id ORDER BY j.created_at DESC LIMIT 1")
            .fetch_one(&r.pool)
            .await
            .unwrap();
        assert_eq!(
            (state.as_str(), code.as_str(), held.as_str(), actual),
            ("failed", "create_failed", "unknown", None),
            "{upstream}"
        );
    }
}
