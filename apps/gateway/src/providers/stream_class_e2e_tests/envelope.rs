//! #665 end to end: a provider error inside a non-stream HTTP 200, behind the
//! real engine and HTTP routers on PostgreSQL, for every adapter and client
//! protocol. A clean recognised rate limit fails over like an HTTP 429; an
//! error next to output or usage, or an ambiguous one without the opt-in,
//! never does. Settlement follows the class: a free pre-processing rejection
//! is a known zero, everything else keeps its hold, and valid usage next to
//! an error is recorded as evidence. The adapters' own parsing is
//! `envelope_class_tests`.
use super::*;
use crate::providers::envelope_class_tests::{envelope, named, with_content, with_usage};

/// A non-stream request of `client`.
fn complete_request(base: &str, client: Client) -> reqwest::RequestBuilder {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    match client {
        Client::Chat => http
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}]})),
        Client::Messages => http
            .post(format!("{base}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(&json!({"model":"company/smart","max_tokens":16,
                "messages":[{"role":"user","content":"hi"}]})),
        Client::Responses => http
            .post(format!("{base}/v1/responses"))
            .json(&json!({"model":"company/smart","input":"hi"})),
    }
}
async fn ask(base: &str, token: &str, client: Client) -> Reply {
    let response = complete_request(base, client)
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    Reply {
        status,
        id,
        body: response.text().await.unwrap(),
    }
}

/// A valid non-stream answer of `kind` (3 input and 2 output tokens).
fn answer(kind: Kind) -> Value {
    match kind {
        Kind::Anthropic => json!({"id":"msg","type":"message","role":"assistant","model":"served",
            "content":[{"type":"text","text":"Hello"}],"stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":3,"output_tokens":2}}),
        Kind::Responses => responses_complete(),
        _ => json!({"id":"c","object":"chat.completion","model":"served",
            "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    }
}

/// Two deployments of `kind` behind one model with explicit failover
/// (`max_attempts` 2) and, when `ambiguous`, the ambiguous-failure opt-in.
async fn two_routes(f: &Fixture, kind: Kind, ambiguous: bool) {
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let mut deployments = Vec::new();
    for (order, model) in [(0, "model-first"), (1, "model-second")] {
        deployments
            .push(add_deployment(f, kind.provider(), kind.credential(), None, model, order).await);
    }
    only_deployments(f, &deployments).await;
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,$2)")
        .bind(f.model)
        .bind(ambiguous)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in deployments.iter().zip([0, 1]) {
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
}

/// What the request of `id` did: the upstream models called, in order, and
/// its executions' (state, error code, input tokens, output tokens).
type Attempt = (String, Option<String>, Option<i64>, Option<i64>);
async fn attempts(pool: &PgPool, id: Uuid) -> Vec<Attempt> {
    sqlx::query_as(
        "SELECT state,error_code,input_tokens,output_tokens FROM inference_executions WHERE root_request_id=$1 ORDER BY started_at,id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The first route answers 200 with `first`, the second with a valid answer.
/// `fails_over` says whether the request must reach the second route; when it
/// does not, the client gets `class` and the one execution records `tokens`.
#[allow(clippy::too_many_arguments)]
async fn first_route_answers(
    f: &Fixture,
    token: &str,
    kind: Kind,
    client: Client,
    first: Value,
    fails_over: bool,
    class: InferenceError,
    tokens: Option<(i64, i64)>,
    at: &str,
    problems: &mut Vec<String>,
) {
    sqlx::query("DELETE FROM deployment_health")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let (log, good) = (seen.clone(), answer(kind).to_string().into_bytes());
    let first = first.to_string().into_bytes();
    let app = Router::new().fallback(move |body: Bytes| {
        let (log, first, good) = (log.clone(), first.clone(), good.clone());
        async move {
            let model = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["model"].as_str().map(str::to_owned))
                .unwrap_or_default();
            log.lock().unwrap().push(model.clone());
            let body = if model == "model-first" { first } else { good };
            response(200, "application/json", vec![Step::Bytes(body)])
        }
    });
    let (mock_base, _mock) = serve(app).await;
    let (base, _gateway) = register(f, kind, &mock_base).await;
    let reply = ask(&base, token, client).await;
    let models = seen.lock().unwrap().clone();
    let recorded = attempts(&f.store.pool, reply.id).await;
    if reply.body.contains("secret") {
        problems.push(format!("{at}: upstream text reached the client"));
    }
    if fails_over {
        let want: Vec<Attempt> = vec![
            ("failed".into(), Some("rate_limit_error".into()), None, None),
            ("succeeded".into(), None, Some(3), Some(2)),
        ];
        if reply.status != 200 || models != ["model-first", "model-second"] || recorded != want {
            problems.push(format!(
                "{at}: status {} models {models:?} attempts {recorded:?}: {}",
                reply.status, reply.body
            ));
        }
    } else {
        let want: Vec<Attempt> = vec![(
            "failed".into(),
            Some(class.code().into()),
            tokens.map(|t| t.0),
            tokens.map(|t| t.1),
        )];
        if reply.status == 200
            || models != ["model-first"]
            || recorded != want
            || !reply.body.contains(shown(client, class))
        {
            problems.push(format!(
                "{at}: status {} models {models:?} attempts {recorded:?}, want {want:?}: {}",
                reply.status, reply.body
            ));
        }
    }
}

/// Failover happens for a clean 200-wrapped rate limit or quota error when a
/// second route exists, exactly as for an HTTP 429, and never when the body
/// carried output or usage, or when the failure is ambiguous and the policy
/// has not opted into ambiguous failover.
async fn failover(pool: PgPool, kind: Kind) {
    let mut problems = Vec::new();
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    two_routes(&f, kind, false).await;
    for client in clients(kind) {
        for name in [
            "rate_limit_exceeded",
            "insufficient_quota",
            "rate_limit_error",
        ] {
            let clean = envelope(kind, named(kind, name));
            let at = format!("{kind:?} {client:?} {name}");
            first_route_answers(
                &f,
                &token,
                kind,
                client,
                clean.clone(),
                true,
                InferenceError::Busy,
                None,
                &format!("{at} clean"),
                &mut problems,
            )
            .await;
            first_route_answers(
                &f,
                &token,
                kind,
                client,
                with_content(kind, clean.clone()),
                false,
                InferenceError::InvalidUpstream,
                None,
                &format!("{at} with output"),
                &mut problems,
            )
            .await;
            first_route_answers(
                &f,
                &token,
                kind,
                client,
                with_usage(kind, clean, false),
                false,
                InferenceError::InvalidUpstream,
                Some((3, 2)),
                &format!("{at} with usage"),
                &mut problems,
            )
            .await;
        }
        // Ambiguous: an unrecognised error is an unavailable provider, which
        // fails over only under the explicit opt-in (off here).
        first_route_answers(
            &f,
            &token,
            kind,
            client,
            envelope(kind, named(kind, "brand_new_error")),
            false,
            InferenceError::UpstreamUnavailable,
            None,
            &format!("{kind:?} {client:?} ambiguous"),
            &mut problems,
        )
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// One deployment of `kind` (free or priced) whose mock answers 200 with the
/// body the test sets.
struct Single {
    f: Fixture,
    token: String,
    deployment: Uuid,
    body: Arc<Mutex<Vec<u8>>>,
    base: String,
    _servers: (
        crate::inference::admission_validation_tests::Server,
        super::super::stream_class_tests::Server,
    ),
}
async fn single(pool: PgPool, kind: Kind, free: bool) -> Single {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let deployment = if free {
        let d = add_unpriced_deployment(
            &f,
            kind.provider(),
            kind.credential(),
            None,
            "private-model",
            0,
        )
        .await;
        free_price(&f.store.pool, d).await;
        d
    } else {
        add_deployment(
            &f,
            kind.provider(),
            kind.credential(),
            None,
            "private-model",
            0,
        )
        .await
    };
    only_deployments(&f, &[deployment]).await;
    let body = Arc::new(Mutex::new(Vec::new()));
    let shared = body.clone();
    let app = Router::new().fallback(move || {
        let body = shared.lock().unwrap().clone();
        async move { response(200, "application/json", vec![Step::Bytes(body)]) }
    });
    let (mock_base, mock) = serve(app).await;
    let (base, server) = register(&f, kind, &mock_base).await;
    Single {
        f,
        token,
        deployment,
        body,
        base,
        _servers: (server, mock),
    }
}

/// Settlement of a failed non-stream attempt follows its class: on a free
/// price, a clean pre-processing rejection is a known zero (as an HTTP 400
/// is); a rate limit, any priced failure, and an error next to usage keep
/// the hold as unknown; valid usage next to the error is recorded, never
/// replaced by zeros.
async fn settlement(pool: PgPool, kind: Kind, free: bool) {
    let mut problems = Vec::new();
    {
        {
            let s = single(pool, kind, free).await;
            for client in clients(kind) {
                let messages = client == Client::Messages;
                let rejected = envelope(kind, named(kind, "invalid_request_error"));
                let cases = [
                    (
                        "a clean rejection",
                        rejected.clone(),
                        http_class(kind, messages, "invalid_request_error", 400),
                        None,
                    ),
                    (
                        "a clean rate limit",
                        envelope(kind, named(kind, "rate_limit_exceeded")),
                        InferenceError::Busy,
                        None,
                    ),
                    (
                        "a rejection next to usage",
                        with_usage(kind, rejected, false),
                        InferenceError::InvalidUpstream,
                        Some((3, 2)),
                    ),
                ];
                for (label, body, class, tokens) in cases {
                    *s.body.lock().unwrap() = body.to_string().into_bytes();
                    sqlx::query("DELETE FROM deployment_health")
                        .execute(&s.f.store.pool)
                        .await
                        .unwrap();
                    let reply = ask(&s.base, &s.token, client).await;
                    let at = format!("{kind:?} {client:?} free={free} {label}");
                    let got = recorded(&s.f.store.pool, s.deployment, reply.id).await;
                    let known_zero = free && class == InferenceError::UpstreamRejected;
                    let want = if known_zero {
                        Recorded {
                            state: "failed".into(),
                            error_code: Some(class.code().into()),
                            input: Some(0),
                            output: Some(0),
                            reservation: "settled".into(),
                            actual: Some(0),
                            ledger: vec!["hold".into(), "settlement".into()],
                            health_failures: 0,
                        }
                    } else {
                        failed_with(class, tokens)
                    };
                    if got != want {
                        problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
                    }
                    if !reply.body.contains(shown(client, class)) || reply.body.contains("secret") {
                        problems.push(format!("{at}: told {}", reply.body));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

macro_rules! per_kind {
    ($($name:ident: $kind:expr,)*) => {$(
        mod $name {
            use super::*;
            #[dbtest::test]
            async fn a_clean_200_rate_limit_fails_over_and_nothing_else_does(pool: PgPool) {
                failover(pool, $kind).await;
            }
            #[dbtest::test]
            async fn settlement_on_a_free_price_follows_the_class_of_a_200_error(pool: PgPool) {
                settlement(pool, $kind, true).await;
            }
            #[dbtest::test]
            async fn settlement_on_a_priced_route_keeps_the_hold_for_a_200_error(pool: PgPool) {
                settlement(pool, $kind, false).await;
            }
        }
    )*};
}
per_kind! {
    anthropic: Kind::Anthropic,
    openai_chat: Kind::OpenAi,
    openai_responses: Kind::Responses,
    openrouter: Kind::OpenRouter,
    local: Kind::Local,
}
