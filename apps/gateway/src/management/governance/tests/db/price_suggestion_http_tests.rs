//! Real loopback HTTP + disposable PostgreSQL proof of the operator handler.
//! Full gateway router, persisted browser sessions and restricted runtime grants.
//! Session rows are seeded; this does not exercise the OIDC callback or SPA.
use super::*;
use crate::identity::tests::MockProvider;
use crate::management::governance::suggestion::{OpenRouterCatalog, tests::mock};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn session(pool: &PgPool, id: Uuid, email: &str) -> (String, String) {
    let cookie = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(email)
        .execute(pool).await.unwrap();
    (format!("omg_session={cookie}"), csrf)
}

#[dbtest::test]
async fn operator_preview_is_bounded_fail_closed_and_preserves_prices(pool: PgPool) {
    let f = fixture(pool).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let all_requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = all_requests.clone();
    let c = calls.clone();
    let wide_responses = Arc::new(AtomicUsize::new(0));
    let wide_hits = wide_responses.clone();
    let image_responses = Arc::new(AtomicUsize::new(0));
    let image_hits = image_responses.clone();
    let catalog = axum::Router::new()
        .route("/models", axum::routing::get(move || {
            let c = c.clone();
            async move {
                // A corrected upstream response must be fetched immediately.
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    return ([("content-type", "application/json")], "{broken".to_owned());
                }
                let tiers: Vec<_> = (1..=20_000).map(|n|json!({"min_prompt_tokens":n,"prompt":"1"})).collect();
                ([("content-type", "application/json")], json!({"data":[
                    {"id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"3","completion":"3","input_cache_write":"1e400","overrides":[{"min_prompt_tokens":1,"input_cache_write":"0"}]}},
                    {"id":"v/large","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","overrides":tiers}},
                    {"id":"v/cold","pricing":{"prompt":"1"}},
                    tiered_model("v/incomplete-free", "0"),
                    tiered_model("v/incomplete-paid", "0.000001"),
                    {"id":"v/image"},
                    {"id":"v/endpoint-large","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","completion":"2"}},
                    {"id":"v/endpoint-deep","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","completion":"2"}},
                    {"id":"v/endpoint-wide","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","completion":"2"}},
                    {"id":"v/tiers","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","overrides":&tiers[..64]}}
                ]}).to_string())
            }
        }))
        .route("/models/v/m/endpoints", axum::routing::get(|| async {
            axum::Json(json!({"endpoints":[
                {"provider_name":"Incomplete","status":0,"pricing":{"prompt":"0"}},
                {"provider_name":"Unknown","status":null,"pricing":{"prompt":"0","completion":"0","request":"0"}},
                {"provider_name":"C\u{202e}lean","status":0,"context_length":0,"max_completion_tokens":0,"pricing":{"prompt":"3","completion":"3","request":"0","input_cache_write":"1e400"}}
            ]}))
        }))
        .route("/models/v/endpoint-large/endpoints", axum::routing::get(|| async {
            axum::Json(json!({"endpoints":[
                {"provider_name":"Example","description":"a".repeat(16_385),"pricing":{"prompt":"1","completion":"2"}}
            ]}))
        }))
        .route("/models/v/endpoint-deep/endpoints", axum::routing::get(|| async {
            let body = format!(r#"{{"endpoints":[],"metadata":{}null{}}}"#, "[".repeat(130), "]".repeat(130));
            ([( "content-type", "application/json")], body)
        }))
        .route("/models/v/endpoint-wide/endpoints", axum::routing::get(move || {
            let wide_hits = wide_hits.clone();
            async move {
                let body = if wide_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    let valid = json!({"endpoints":[{"provider_name":"Example","status":0,"pricing":{"prompt":"1","completion":"2"}}]}).to_string();
                    format!(r#"{{{}{}"#, r#""metadata":0,"#.repeat(64), &valid[1..])
                } else {
                    json!({"endpoints":[{"provider_name":"Recovered","status":0,"pricing":{"prompt":"3","completion":"3"}}]}).to_string()
                };
                ([("content-type", "application/json")], body)
            }
        }))
        .route("/images/models/v/image/endpoints", axum::routing::get(move || {
            let attempt = image_hits.fetch_add(1, Ordering::SeqCst);
            async move {
                let body = if attempt == 0 {
                    r#"{"data":[[]]}"#
                } else {
                    r#"{"data":null,"data":null,"endpoints":[{"pricing":[{"billable":"output_image","unit":"image","cost_usd":"0.01"}]}]}"#
                };
                ([("content-type", "application/json")], body)
            }
        }))
        .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            observed_requests.fetch_add(1, Ordering::SeqCst);
            assert!(req.headers().get("authorization").is_none());
            assert!(req.headers().get("cookie").is_none());
            async move { next.run(req).await }
        }));
    let (catalog_url, catalog_task) = mock(catalog).await;
    let _catalog_server = Server(catalog_task);
    let catalog = Arc::new(OpenRouterCatalog::for_test(
        catalog_url,
        std::time::Duration::from_secs(2),
    ));
    let (connection, model, deployment, large, tiered, cold) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Preview','openrouter','env:NOT_USED','https://openrouter.ai/api/v1',false)").bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'preview',ARRAY['chat_completions'])").bind(model).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$3,$4,'v/m'),($2,$3,$4,'v/large'),($5,$3,$4,'v/tiers'),($6,$3,$4,'v/cold')").bind(deployment).bind(large).bind(model).bind(connection).bind(tiered).bind(cold).execute(&f.store.pool).await.unwrap();
    let incomplete_tiers = [
        (Uuid::new_v4(), "v/incomplete-free", "0"),
        (Uuid::new_v4(), "v/incomplete-paid", "1000000"),
    ];
    for (id, upstream, _) in incomplete_tiers {
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,$4)")
            .bind(id).bind(model).bind(connection).bind(upstream).execute(&f.store.pool).await.unwrap();
    }
    let endpoint_large = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'v/endpoint-large')")
        .bind(endpoint_large).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
    let endpoint_deep = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'v/endpoint-deep')")
        .bind(endpoint_deep).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
    let endpoint_wide = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'v/endpoint-wide')")
        .bind(endpoint_wide).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
    let image_model = Uuid::new_v4();
    let image_deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'preview-image',ARRAY['images'])")
        .bind(image_model).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'v/image')")
        .bind(image_deployment).bind(image_model).bind(connection).execute(&f.store.pool).await.unwrap();
    // Create an actual immutable price first. Preview is read-only even when
    // parsing fails or refuses a hostile entry.
    let admin = user(f.owner, true);
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/prices"),
            price()
        )
        .await
        .0,
        StatusCode::OK
    );
    let before: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM deployment_prices p ORDER BY id")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(before.len(), 1);
    let (admin_cookie, csrf) = session(&f.store.pool, f.owner, "owner@test.invalid").await;
    let (member_cookie, _) = session(&f.store.pool, f.other, "member@test.invalid").await;
    // The allowlist and SET ROLE match the runtime HTTP acceptance fixtures.
    sqlx::raw_sql(include_str!(
        "../../../../../tests/common/ensure_runtime_role.sql"
    ))
    .execute(&f.store.pool)
    .await
    .unwrap();
    f.store.apply_runtime_grants().await.unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime; SET omg.scope_lock_audit=on")
                    .await?;
                Ok(())
            })
        })
        .connect_with((*f.store.pool.connect_options()).clone())
        .await
        .unwrap();
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&runtime)
        .await
        .unwrap();
    assert_eq!(role, "gateway_runtime");
    let restricted: bool = sqlx::query_scalar("SELECT NOT (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls) FROM pg_roles WHERE rolname=current_user")
        .fetch_one(&runtime).await.unwrap();
    assert!(
        restricted,
        "HTTP fixture requires an unprivileged runtime role"
    );
    let mock_oidc = MockProvider::start(None).await;
    let identity = mock_oidc.state(runtime.clone()).await;
    let store = Store::new(runtime.clone());
    let engine = crate::inference::Engine::new(
        Arc::new(store.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app =
        crate::http::router_with_identity(store, None, engine, identity).layer(Extension(catalog));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("http://{}", listener.local_addr().unwrap());
    let _gateway_server = Server(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("{gateway_url}/api/v1/platform/deployments/{deployment}/price-suggestion");
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&url)
            .header("cookie", &member_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let inference = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'preview refusal',$4)")
        .bind(inference.id).bind(f.principal.workspace_id).bind(f.owner)
        .bind(inference.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&inference.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        all_requests.load(Ordering::SeqCst),
        0,
        "refused principals cannot fetch the catalog"
    );
    let r = client
        .get(&url)
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["message"],
        "OpenRouter catalog unavailable"
    );
    let r = client
        .get(&url)
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let out: Value = r.json().await.unwrap();
    assert_eq!(out["endpoint"]["selection"], "cheapest");
    assert_eq!(out["endpoint"]["provider"], "Clean");
    assert_eq!(out["draft"]["output_token_limit"], 100);
    assert_eq!(out["draft"]["input_token_limit"], 4096);
    let lines = out["draft"]["price_lines"].as_array().unwrap();
    for meter in ["input_tokens", "output_tokens"] {
        let line = lines.iter().find(|p| p["meter"] == meter).unwrap();
        assert_eq!(line["batch"], 1_000_000);
        assert_eq!(line["microusd_per_batch"], "3000000000000");
    }
    assert!(
        lines
            .iter()
            .any(|p| p["meter"] == "cache_write_tokens" && p["unknown"] == true)
    );
    assert!(
        out["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["meter"] == "cache_write_tokens" && p["min_prompt_tokens"] == 1)
    );
    assert_eq!(out["needs_review"], true);
    let fetched = all_requests.load(Ordering::SeqCst);
    let repeated = client
        .get(&url)
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(repeated.status(), StatusCode::OK);
    assert_eq!(repeated.json::<Value>().await.unwrap(), out);
    assert_eq!(all_requests.load(Ordering::SeqCst), fetched);
    for refused in [large, tiered, endpoint_large, endpoint_deep, endpoint_wide] {
        let r = client
            .get(format!(
                "{gateway_url}/api/v1/platform/deployments/{refused}/price-suggestion"
            ))
            .header("cookie", &admin_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = r.bytes().await.unwrap();
        assert!(bytes.len() < 16_384);
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["needs_review"], true);
        assert!(out["lines"].as_array().unwrap().len() <= 64);
        assert!(out["warnings"].as_array().unwrap().len() <= 64);
        assert!(!out["warnings"].to_string().contains("Priced from"));
        let refused_lines = out["draft"]["price_lines"].as_array().unwrap();
        assert!(!refused_lines.is_empty());
        assert!(refused_lines.iter().all(|p| p["unknown"] == true));
        assert_eq!(out["draft"]["input_token_limit"], 4096);
        assert_eq!(out["draft"]["output_token_limit"], 100);
        assert!(out.get("endpoint").is_none());
        assert!(!out["warnings"].to_string().contains("price was used"));
    }
    let corrected_url =
        format!("{gateway_url}/api/v1/platform/deployments/{endpoint_wide}/price-suggestion");
    for _ in 0..2 {
        let corrected = client
            .get(&corrected_url)
            .header("cookie", &admin_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(corrected.status(), StatusCode::OK);
        let body: Value = corrected.json().await.unwrap();
        assert_eq!(body["endpoint"]["provider"], "Recovered");
    }
    assert_eq!(wide_responses.load(Ordering::SeqCst), 2);
    let image_url =
        format!("{gateway_url}/api/v1/platform/deployments/{image_deployment}/price-suggestion");
    let invalid = client
        .get(&image_url)
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_GATEWAY);
    for _ in 0..2 {
        let recovered = client
            .get(&image_url)
            .header("cookie", &admin_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(recovered.status(), StatusCode::OK);
        let body: Value = recovered.json().await.unwrap();
        assert!(
            body["lines"].as_array().unwrap().iter().any(|line| {
                line["meter"] == "output_images" && line["microusd_per_batch"] == "10000"
            }),
            "{body}"
        );
    }
    assert_eq!(image_responses.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let r = client
        .get(format!(
            "{gateway_url}/api/v1/platform/deployments/{}/price-suggestion",
            f.deployment
        ))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = client
        .get(format!(
            "{gateway_url}/api/v1/platform/deployments/{}/price-suggestion",
            Uuid::new_v4()
        ))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let mut published = Vec::new();
    let mut imported_input = Vec::new();
    for (id, _, base_amount) in incomplete_tiers {
        let deployment_url = format!("{gateway_url}/api/v1/platform/deployments/{id}");
        let preview = client
            .get(format!("{deployment_url}/price-suggestion"))
            .header("cookie", &admin_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        let preview: Value = preview.json().await.unwrap();
        let publish = |body: Value| {
            client
                .post(format!("{deployment_url}/prices"))
                .header("cookie", &admin_cookie)
                .header("origin", "http://127.0.0.1:3000")
                .header("x-csrf-token", &csrf)
                .json(&body)
                .send()
        };
        let draft = &preview["draft"];
        let mut omitted = draft.clone();
        omitted["price_lines"]
            .as_array_mut()
            .unwrap()
            .retain(|line| line["meter"] != "input_tokens");
        let refusal = publish(omitted).await.unwrap();
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
        let error = refusal.json::<Value>().await.unwrap();
        assert_eq!(error["error"]["reason"], "price_meters_incomplete");
        assert_eq!(error["error"]["missing_meters"], json!(["input_tokens"]));

        // Direct clients can submit the draft as-is. The unknown meter must
        // be stored without any rate, so larger prompts cannot use the base.
        let response = publish(draft.clone()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let price_id: Uuid =
            serde_json::from_value(response.json::<Value>().await.unwrap()["id"].clone()).unwrap();
        published.push(price_id);

        assert_eq!(preview["needs_review"], true);
        let annotated = preview["lines"].as_array().unwrap();
        for (threshold, amount) in [
            (Value::Null, json!(base_amount)),
            (json!(200), Value::Null),
            (json!(300), json!("2000000")),
        ] {
            assert!(
                annotated.iter().any(|line| line["meter"] == "input_tokens"
                    && line["min_prompt_tokens"] == threshold
                    && line["microusd_per_batch"] == amount),
                "{preview}"
            );
        }
        let draft_input: Vec<_> = draft["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|line| line["meter"] == "input_tokens")
            .cloned()
            .collect();
        let stored: Vec<Value> =
            sqlx::query_scalar("SELECT price_lines FROM deployment_prices WHERE deployment_id=$1")
                .bind(id)
                .fetch_all(&f.store.pool)
                .await
                .unwrap();
        assert_eq!(stored.len(), 1);
        let stored_lines = stored[0].as_array().unwrap();
        let stored_input: Vec<_> = stored_lines
            .iter()
            .filter(|line| line["meter"] == "input_tokens")
            .cloned()
            .collect();
        imported_input.push((base_amount, draft_input, stored_input));
        assert!(
            stored_lines
                .iter()
                .any(|line| line["meter"] == "output_tokens"
                    && line["min_prompt_tokens"] == 200
                    && line["microusd_per_batch"] == "3000000")
        );
    }
    let unknown_input = vec![json!({"meter":"input_tokens","unknown":true})];
    assert_eq!(
        imported_input,
        [
            ("0", unknown_input.clone(), vec![]),
            ("1000000", unknown_input, vec![]),
        ]
    );
    let uncached_url = format!("{gateway_url}/api/v1/platform/deployments/{cold}/price-suggestion");
    let before_denials = all_requests.load(Ordering::SeqCst);
    // A stale browser session cannot retain admin authority after revocation.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(f.owner)
    .execute(&f.store.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(f.owner)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let r = client
        .get(&uncached_url)
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'auditor','manual')",
    )
    .bind(f.other)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let r = client
        .get(&uncached_url)
        .header("cookie", &member_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(all_requests.load(Ordering::SeqCst), before_denials);
    let after: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM deployment_prices p WHERE NOT (id=ANY($1)) ORDER BY id",
    )
    .bind(&published)
    .fetch_all(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    runtime.close().await;
}

fn tiered_model(id: &str, prompt: &str) -> Value {
    json!({
        "id": id,
        "context_length": 4096,
        "top_provider": {"max_completion_tokens": 100},
        "pricing": {
            "prompt": prompt,
            "completion": "0.000001",
            "overrides": [
                {"min_prompt_tokens": 200, "prompt": "1e400", "completion": "0.000003"},
                {"min_prompt_tokens": 300, "prompt": "0.000002"}
            ]
        }
    })
}
