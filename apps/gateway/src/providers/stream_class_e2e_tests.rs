//! #614 and #622 end to end: the real adapters against a mock upstream that
//! answers 200 and then fails inside the stream, behind the real engine and
//! HTTP routers on PostgreSQL. The response stays a 200 that ends in an error
//! (the gateway does not wait for content and never fails over after a stream
//! is returned); what the failure decides is its class: the terminal error
//! each client protocol sees, the recorded execution, the reservation and the
//! routing-health observation. The adapters' own parsing is
//! `stream_class_tests`; Bedrock's is `bedrock::stream_error_tests`.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Router, body::Bytes};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

use super::stream_class_tests::{
    Kind, NAMES, Step, http_class, response, responses_complete, serve, sse, terminal_order_cases,
};
use crate::{
    billing::v3::Meter,
    governance::tests::db::{Fixture, fixture},
    inference::{
        admission_validation_tests::{
            add_deployment, add_unpriced_deployment, issue_key, start_gateway,
        },
        error::InferenceError,
    },
    providers::ProviderRegistry,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    Chat,
    Messages,
    Responses,
}
/// The client protocols that reach `kind`: Responses reaches the native
/// Responses transport only; local profiles use pinned approved endpoints.
fn clients(kind: Kind) -> Vec<Client> {
    match kind {
        Kind::Responses => vec![Client::Responses],
        _ => vec![Client::Chat, Client::Messages],
    }
}

/// A gateway over one deployment of `kind` whose mock answers with `script`.
struct World {
    f: Fixture,
    token: String,
    deployment: Uuid,
    script: Arc<Mutex<Vec<Step>>>,
    base: String,
    _servers: Vec<crate::inference::admission_validation_tests::Server>,
    _mock: super::stream_class_tests::Server,
}
async fn register(
    f: &Fixture,
    kind: Kind,
    mock_base: &str,
) -> (String, crate::inference::admission_validation_tests::Server) {
    if kind == Kind::Local {
        sqlx::query("UPDATE provider_connections SET endpoint=$1 WHERE provider='vllm'")
            .bind(kind.target(mock_base).endpoint)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    let mut registry = ProviderRegistry::default();
    registry.register(kind.adapter(mock_base)).unwrap();
    start_gateway(f, registry).await
}
async fn world(pool: PgPool, kind: Kind, free: bool) -> World {
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
    let script = Arc::new(Mutex::new(Vec::<Step>::new()));
    let shared = script.clone();
    let app = Router::new().fallback(move || {
        let steps = shared.lock().unwrap().clone();
        async move { response(200, "text/event-stream", steps) }
    });
    let (mock_base, mock) = serve(app).await;
    let (base, server) = register(&f, kind, &mock_base).await;
    World {
        f,
        token,
        deployment,
        script,
        base,
        _servers: vec![server],
        _mock: mock,
    }
}

/// The fixture's model has a deployment of its own (an OpenAI one, which
/// would be picked first for `openai`): leave only `keep` enabled.
async fn only_deployments(f: &Fixture, keep: &[Uuid]) {
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1 AND id<>ALL($2)")
        .bind(f.model)
        .bind(keep)
        .execute(&f.store.pool)
        .await
        .unwrap();
}

/// An explicitly free pricing v3 version: the shape a free pre-processing
/// rejection settles against (`governance::free_rejection`).
async fn free_price(pool: &PgPool, deployment: Uuid) {
    let lines: Vec<Value> = Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputTokens | Meter::OutputTokens | Meter::Requests | Meter::SearchUnits => {
                let batch = m.batches()[0];
                json!({"meter":m.as_str(),"microusd_per_batch":"0","batch":batch,"unit_label":m.unit_label(batch).unwrap(),"sku_label":"Free"})
            }
            m => json!({"meter":m.as_str(),"not_applicable":true}),
        })
        .collect();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(pool).await.unwrap();
}

struct Reply {
    status: u16,
    id: Uuid,
    body: String,
}
fn client_request(base: &str, client: Client) -> reqwest::RequestBuilder {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    match client {
        Client::Chat => http
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({
                "model":"company/smart","stream":true,"stream_options":{"include_usage":true},
                "messages":[{"role":"user","content":"hi"}]
            })),
        Client::Messages => http
            .post(format!("{base}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model":"company/smart","max_tokens":16,"stream":true,
                "messages":[{"role":"user","content":"hi"}]
            })),
        Client::Responses => http.post(format!("{base}/v1/responses")).json(&json!({
            "model":"company/smart","input":"hi","stream":true
        })),
    }
}
async fn call(base: &str, token: &str, client: Client) -> Reply {
    let response = client_request(base, client)
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
impl World {
    async fn ask_hanging(&self, client: Client, wire: Vec<u8>, at: &str) -> Reply {
        let dropped = Arc::new(Notify::new());
        // The reset runs first: its own wait is not the request's time.
        self.prepare(vec![Step::BytesThenHang(wire, dropped.clone())])
            .await;
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            call(&self.base, &self.token, client),
        )
        .await
        .unwrap_or_else(|_| panic!("{at}: gateway waited for upstream EOF"));
        tokio::time::timeout(Duration::from_secs(5), dropped.notified())
            .await
            .unwrap_or_else(|_| panic!("{at}: upstream body stayed open"));

        reply
    }

    async fn ask(&self, client: Client, steps: Vec<Step>) -> Reply {
        self.prepare(steps).await;
        call(&self.base, &self.token, client).await
    }
    /// The next upstream answer, with routing health cleared.
    async fn prepare(&self, steps: Vec<Step>) {
        *self.script.lock().unwrap() = steps;
        reset_health(&self.f.store.pool).await;
    }
}

/// Waits until every statement the test pool started has finished: a
/// completion barrier that takes all of the pool's connections at once (the
/// gateway under test shares this pool). A health observation that outlives
/// its 250 ms advisory timeout (`ExecutionGuard::finish`) is abandoned by its
/// request, but its single statement is already on the server and commits
/// after the reply ended; its connection returns to the pool only once that
/// statement's reply was drained. So a read after the barrier sees the case's
/// own observation, and a reset after it never precedes a straggler, which
/// would otherwise land in the next case. One 10 s deadline bounds the whole
/// barrier: a statement still running then is a failure of its own. A write
/// whose timeout expired before it was sent (waiting for a connection) never
/// happens, and its case's exact count still fails.
async fn health_writes_settled(pool: &PgPool) {
    let size = pool.options().get_max_connections() as usize;
    let held = tokio::time::timeout(Duration::from_secs(10), async {
        let mut held = Vec::with_capacity(size);
        for _ in 0..size {
            held.push(pool.acquire().await.unwrap());
        }
        held
    })
    .await
    .expect("a statement on the test pool was still running after 10 s");
    drop(held);
}
/// Clears routing health for the next case, after every earlier write landed.
async fn reset_health(pool: &PgPool) {
    health_writes_settled(pool).await;
    sqlx::query("DELETE FROM deployment_health")
        .execute(pool)
        .await
        .unwrap();
}

/// The last `data:` payload of an SSE body that is JSON.
fn last_event(body: &str) -> Value {
    body.lines()
        .rev()
        .find_map(|line| line.strip_prefix("data:"))
        .and_then(|data| serde_json::from_str(data.trim()).ok())
        .unwrap_or_else(|| panic!("no JSON event in {body}"))
}
/// The error the client was told, in its protocol's own field.
fn told(client: Client, body: &str) -> String {
    let event = last_event(body);
    match client {
        Client::Chat => event["error"]["code"].as_str(),
        Client::Messages => event["error"]["type"].as_str(),
        Client::Responses => event["code"].as_str(),
    }
    .unwrap_or_else(|| panic!("no error in {body}"))
    .to_owned()
}
/// What each client protocol is told for each class (`protocols::*` error
/// bodies), restated as an oracle.
fn shown(client: Client, class: InferenceError) -> &'static str {
    match (client, class) {
        (Client::Messages, InferenceError::Busy) => "rate_limit_error",
        (Client::Messages, InferenceError::UpstreamUnavailable | InferenceError::Overloaded) => {
            "overloaded_error"
        }
        (Client::Messages, InferenceError::UpstreamRejected | InferenceError::ContextLength) => {
            "invalid_request_error"
        }
        (Client::Messages, _) => "api_error",
        (_, class) => class.code(),
    }
}
/// The classes that count against routing health.
fn counts(class: InferenceError) -> bool {
    matches!(
        class,
        InferenceError::Busy
            | InferenceError::Overloaded
            | InferenceError::UpstreamUnavailable
            | InferenceError::Timeout
    )
}

/// What one request left behind.
#[derive(Debug, PartialEq)]
struct Recorded {
    state: String,
    error_code: Option<String>,
    input: Option<i64>,
    output: Option<i64>,
    reservation: String,
    actual: Option<i64>,
    ledger: Vec<String>,
    health_failures: i64,
}
/// state, error code, input and output tokens, reservation state, actual cost, id.
type Row = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
    Option<i64>,
    Uuid,
);
async fn recorded(pool: &PgPool, deployment: Uuid, request: Uuid) -> Recorded {
    let (state, error_code, input, output, reservation, actual, id): Row = sqlx::query_as(
        "SELECT e.state,e.error_code,e.input_tokens,e.output_tokens,r.state,r.actual_microusd,e.id FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.root_request_id=$1",
    )
    .bind(request)
    .fetch_one(pool)
    .await
    .unwrap();
    let ledger = sqlx::query_scalar::<_, String>(
        "SELECT kind FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    health_writes_settled(pool).await;
    let health_failures: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT consecutive_failures FROM deployment_health WHERE deployment_id=$1),0)::bigint",
    )
    .bind(deployment)
    .fetch_one(pool)
    .await
    .unwrap();
    Recorded {
        state,
        error_code,
        input,
        output,
        reservation,
        actual,
        ledger,
        health_failures,
    }
}
/// A returned stream that failed: a failed execution, the hold retained
/// (`unknown`, nothing settled), no usage invented, and the health observation
/// of its class. `tokens` are the counts the stream reported before it failed.
fn failed_with(class: InferenceError, tokens: Option<(i64, i64)>) -> Recorded {
    Recorded {
        state: "failed".into(),
        error_code: Some(class.code().into()),
        input: tokens.map(|t| t.0),
        output: tokens.map(|t| t.1),
        reservation: "unknown".into(),
        actual: None,
        ledger: vec!["hold".into(), "unknown".into()],
        health_failures: i64::from(counts(class)),
    }
}
fn expected_class(kind: Kind, client: Client, name: &str) -> InferenceError {
    let status = NAMES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
    match status {
        Some(status) => http_class(kind, client == Client::Messages, name, status),
        None => InferenceError::UpstreamUnavailable,
    }
}

/// Checks one failed stream against the class it must have ended in.
async fn check(
    w: &World,
    client: Client,
    reply: &Reply,
    class: InferenceError,
    tokens: Option<(i64, i64)>,
    at: &str,
    problems: &mut Vec<String>,
) {
    if reply.status != 200 {
        problems.push(format!("{at}: status {}", reply.status));
    }
    let shown = shown(client, class);
    if told(client, &reply.body) != shown {
        problems.push(format!(
            "{at}: client {}, want {shown}",
            told(client, &reply.body)
        ));
    }
    for finished in ["[DONE]", "message_stop", "response.completed"] {
        if reply.body.contains(finished) {
            problems.push(format!("{at}: a finish was sent"));
        }
    }
    if reply.body.contains("secret") {
        problems.push(format!("{at}: upstream text reached the client"));
    }
    let got = recorded(&w.f.store.pool, w.deployment, reply.id).await;
    let want = failed_with(class, tokens);
    if got != want {
        problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
    }
}

#[dbtest::test]
async fn anthropic_stop_reason_controls_stream_success_and_settlement(pool: PgPool) {
    let kind = Kind::Anthropic;
    let w = world(pool, kind, false).await;
    let (content, _, stop) = kind.valid();
    let wire = |reason| {
        let delta = sse(&[
            json!({"type":"content_block_stop","index":0}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":reason,"stop_sequence":null},
                "usage":{"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }),
        ]);
        [content.as_slice(), &delta, &stop].concat()
    };
    let events = |body: &str| -> Vec<Value> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    };
    let invalid = wire("unknown_reason");
    let valid = wire("end_turn");

    for client in clients(kind) {
        let reply = w.ask(client, vec![Step::Bytes(invalid.clone())]).await;
        let mut problems = Vec::new();
        check(
            &w,
            client,
            &reply,
            InferenceError::InvalidUpstream,
            None,
            "invalid Anthropic stop reason",
            &mut problems,
        )
        .await;
        assert!(problems.is_empty(), "{client:?}: {problems:#?}");
        assert!(reply.body.contains("Hello"), "{client:?}");
        let refused = events(&reply.body);
        assert_eq!(
            refused.iter().filter(|e| e.get("error").is_some()).count(),
            1
        );
        assert!(refused.iter().all(|e| {
            e["usage"].is_null()
                && e["choices"][0]["finish_reason"].is_null()
                && e["delta"]["stop_reason"].is_null()
        }));
        let (held, admitted): (i64, i64) = sqlx::query_as(
            "SELECT r.held_microusd,h.amount_microusd FROM inference_executions e
             JOIN governance_reservations r ON r.execution_id=e.id AND r.admitted_at=e.started_at
             JOIN monetary_ledger h ON h.execution_id=e.id AND h.admitted_at=e.started_at AND h.kind='hold'
             WHERE e.root_request_id=$1",
        )
        .bind(reply.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert!(admitted > 0);
        assert_eq!(held, admitted, "{client:?}");

        let reply = w.ask(client, vec![Step::Bytes(valid.clone())]).await;
        assert_eq!(reply.status, 200, "{client:?}");
        let terminal = if client == Client::Messages {
            "event: message_stop"
        } else {
            "data: [DONE]"
        };
        assert_eq!(
            reply.body.lines().filter(|line| *line == terminal).count(),
            1
        );
        let (before_terminal, _) = reply.body.split_once(terminal).unwrap();
        let accepted = events(before_terminal);
        if client == Client::Messages {
            let finish = accepted
                .iter()
                .find(|e| e["delta"]["stop_reason"] == "end_turn")
                .unwrap();
            assert_eq!(finish["usage"]["input_tokens"], 4);
            assert_eq!(finish["usage"]["output_tokens"], 2);
        } else {
            assert!(
                accepted
                    .iter()
                    .any(|e| e["choices"][0]["finish_reason"] == "stop")
            );
            let usage = accepted.iter().find(|e| e["usage"].is_object()).unwrap();
            assert_eq!(usage["usage"]["prompt_tokens"], 4);
            assert_eq!(usage["usage"]["completion_tokens"], 2);
        }
        assert_eq!(
            recorded(&w.f.store.pool, w.deployment, reply.id).await,
            Recorded {
                state: "succeeded".into(),
                error_code: None,
                input: Some(4),
                output: Some(2),
                reservation: "settled".into(),
                actual: Some(6),
                ledger: vec!["hold".into(), "settlement".into()],
                health_failures: 0,
            },
            "{client:?}"
        );
    }
}

/// Every in-stream error name, each client protocol, before and after
/// content: the 200 ends in the class the same error has as an HTTP response,
/// and nothing about settlement changes.
async fn in_stream_errors(pool: PgPool, kind: Kind, free: bool, names: &[&str]) {
    let w = world(pool, kind, free).await;
    let mut problems = Vec::new();
    let (content, _, _) = kind.valid();
    for client in clients(kind) {
        for name in names {
            let class = expected_class(kind, client, name);
            for frame in kind.errors(name) {
                for (place, prefix) in [
                    ("before content", Vec::new()),
                    ("after content", content.clone()),
                ] {
                    let steps = vec![Step::Bytes(
                        [prefix, sse(std::slice::from_ref(&frame))].concat(),
                    )];
                    let reply = w.ask(client, steps).await;
                    let at = format!("{kind:?} {client:?} {name} {place} free={free}");
                    check(&w, client, &reply, class, None, &at, &mut problems).await;
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
const NAMES_UNDER_TEST: &[&str] = &[
    "rate_limit_error",
    "rate_limit_exceeded",
    "api_error",
    "server_error",
    "overloaded_error",
    "invalid_request_error",
    "context_length_exceeded",
    "authentication_error",
    "brand_new_error_name",
];
/// The client-fault names: on a free price too, a rejection inside a returned
/// stream is not a rejection before processing, so it never settles as a
/// known zero.
const REJECTIONS: &[&str] = &["invalid_request_error", "context_length_exceeded"];

macro_rules! per_kind {
    ($($name:ident: $kind:expr,)*) => {$(
        mod $name {
            use super::*;
            #[dbtest::test]
            async fn in_stream_errors_end_with_the_class_of_the_same_http_error(pool: PgPool) {
                in_stream_errors(pool, $kind, false, NAMES_UNDER_TEST).await;
            }
            #[dbtest::test]
            async fn in_stream_rejections_on_a_free_price_never_settle_as_a_known_zero(pool: PgPool) {
                in_stream_errors(pool, $kind, true, REJECTIONS).await;
            }
            #[dbtest::test]
            async fn transport_failures_inside_a_stream_are_unavailable_and_count(pool: PgPool) {
                transport_failures(pool, $kind).await;
            }
            #[dbtest::test]
            async fn a_failed_stream_is_never_failed_over(pool: PgPool) {
                never_failed_over(pool, $kind).await;
            }
            #[dbtest::test]
            async fn the_breaker_follows_the_class_of_in_stream_failures(pool: PgPool) {
                breaker(pool, $kind).await;
            }
            #[dbtest::test]
            async fn a_client_that_goes_away_mid_stream_is_not_a_provider_failure(pool: PgPool) {
                client_drop(pool, $kind).await;
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

#[dbtest::test]
async fn responses_first_terminal_wins_and_closes_the_upstream_body(pool: PgPool) {
    terminal_order(pool, Kind::Responses, Client::Responses, 3).await;
}

#[dbtest::test]
async fn anthropic_first_terminal_wins_and_closes_the_upstream_body(pool: PgPool) {
    terminal_order(pool, Kind::Anthropic, Client::Messages, 4).await;
}

#[dbtest::test]
async fn anthropic_chat_first_terminal_wins_and_closes_the_upstream_body(pool: PgPool) {
    terminal_order(pool, Kind::Anthropic, Client::Chat, 4).await;
}

async fn terminal_order(pool: PgPool, kind: Kind, client: Client, input: i64) {
    let w = world(pool, kind, false).await;
    let terminal_line = match client {
        Client::Chat => "data: [DONE]",
        Client::Messages => "event: message_stop",
        Client::Responses => "event: response.completed",
    };
    for (label, wire, failed) in terminal_order_cases(kind) {
        let at = format!("{kind:?} {label}, error={failed:?}");
        let reply = w.ask_hanging(client, wire, &at).await;

        assert_eq!(reply.status, 200, "{at}");
        assert!(reply.body.contains("Hello"), "{at}");
        assert!(!reply.body.contains("late text"), "{at}");
        assert_eq!(
            reply
                .body
                .lines()
                .filter(|line| *line == terminal_line)
                .count(),
            usize::from(failed.is_none()),
            "{at}"
        );
        let got = recorded(&w.f.store.pool, w.deployment, reply.id).await;
        if let Some(expected) = failed {
            assert_eq!(told(client, &reply.body), shown(client, expected), "{at}");
            assert_eq!(got, failed_with(expected, None), "{at}");
        } else {
            match client {
                Client::Chat => assert_eq!(
                    reply.body.trim_end().lines().last(),
                    Some(terminal_line),
                    "{at}"
                ),
                Client::Messages => {
                    assert_eq!(last_event(&reply.body)["type"], "message_stop", "{at}")
                }
                Client::Responses => assert_eq!(
                    last_event(&reply.body)["type"],
                    "response.completed",
                    "{at}"
                ),
            }
            assert_eq!(got.state, "succeeded", "{at}");
            assert_eq!(got.error_code, None, "{at}");
            assert_eq!((got.input, got.output), (Some(input), Some(2)), "{at}");
            assert_eq!(got.reservation, "settled", "{at}");
            assert_eq!(got.actual, Some(input + 2), "{at}");
            assert_eq!(got.ledger, ["hold", "settlement"], "{at}");
            if client == Client::Responses {
                assert_eq!(
                    last_event(&reply.body)["response"]["usage"],
                    json!({
                        "input_tokens": input, "output_tokens": 2, "total_tokens": input + 2
                    }),
                    "{at}"
                );
            }
        }
    }
}

#[dbtest::test]
async fn responses_terminal_status_mismatch_preserves_valid_usage(pool: PgPool) {
    responses_rejected_terminal_evidence(pool, false).await;
}

#[dbtest::test]
async fn responses_terminal_content_mismatch_preserves_valid_usage(pool: PgPool) {
    responses_rejected_terminal_evidence(pool, true).await;
}

async fn responses_rejected_terminal_evidence(pool: PgPool, matching_status: bool) {
    let kind = Kind::Responses;
    let w = world(pool, kind, false).await;
    let (content, rest, late_completion) = kind.valid();
    let prefix = [content, rest].concat();
    let snapshot = responses_complete();
    let class = InferenceError::InvalidUpstream;
    let mut problems = Vec::new();
    for (event, terminal_status, other_status) in [
        ("response.incomplete", "incomplete", "completed"),
        ("response.completed", "completed", "incomplete"),
    ] {
        let mut snapshot = snapshot.clone();
        let status = if matching_status {
            terminal_status
        } else {
            other_status
        };
        snapshot["status"] = json!(status);
        if matching_status {
            snapshot["output"][0]["content"][0]["text"] = json!("Different final text");
        }
        if status == "incomplete" {
            snapshot["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        snapshot.as_object_mut().unwrap().remove("usage");
        let mut wire_snapshot = snapshot.to_string();
        wire_snapshot.pop(); // Append raw fields before the closing brace.
        // The fixture prices input and output at one micro-USD per token.
        for (label, usage, input, output, expected_floor) in [
            (
                "reported",
                r#","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}"#,
                Some(3),
                Some(2),
                5,
            ),
            (
                "partial input",
                r#","usage":{"input_tokens":3,"total_tokens":5}"#,
                Some(3),
                None,
                3,
            ),
            (
                "partial output",
                r#","usage":{"output_tokens":2,"total_tokens":5}"#,
                None,
                Some(2),
                2,
            ),
            (
                "explicit zeros",
                r#","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}"#,
                Some(0),
                Some(0),
                0,
            ),
            ("missing", "", None, None, 0),
            (
                "total only",
                r#","usage":{"total_tokens":5}"#,
                None,
                None,
                0,
            ),
            (
                "invalid",
                r#","usage":{"input_tokens":3,"output_tokens":-2}"#,
                None,
                None,
                0,
            ),
            (
                "contradictory total",
                r#","usage":{"input_tokens":5000,"output_tokens":2,"total_tokens":5001}"#,
                None,
                None,
                0,
            ),
            (
                "impossible partial",
                r#","usage":{"output_tokens":2,"total_tokens":1}"#,
                None,
                None,
                0,
            ),
            (
                "cache evidence exceeds partial total",
                r#","usage":{"output_tokens":2,"input_tokens_details":{"cached_tokens":5000},"total_tokens":5001}"#,
                None,
                None,
                0,
            ),
            (
                "cache evidence fits partial total",
                r#","usage":{"output_tokens":2,"input_tokens_details":{"cached_tokens":5000},"total_tokens":5002}"#,
                None,
                Some(2),
                5002,
            ),
            (
                "object-valued count",
                r#","usage":{"input_tokens":{"count":3},"output_tokens":2}"#,
                None,
                None,
                0,
            ),
            (
                "conflicting reasoning fields",
                r#","usage":{"input_tokens":3,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1},"completion_tokens_details":{"reasoning_tokens":2}}"#,
                Some(3),
                Some(2),
                5,
            ),
            (
                "reasoning exceeds total less input",
                r#","usage":{"input_tokens":3,"total_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}"#,
                Some(3),
                None,
                3,
            ),
            (
                "reasoning exceeds total less cache floor",
                r#","usage":{"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":1},"total_tokens":5,"completion_tokens_details":{"reasoning_tokens":2}}"#,
                None,
                None,
                4,
            ),
            (
                "reasoning fits total less cache floor",
                r#","usage":{"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":1},"total_tokens":5,"output_tokens_details":{"reasoning_tokens":1}}"#,
                None,
                None,
                4,
            ),
            (
                "duplicate reasoning count",
                r#","usage":{"input_tokens":5000,"output_tokens":2,"total_tokens":5002,"output_tokens_details":{"reasoning_tokens":1,"reasoning_tokens":2}}"#,
                Some(5000),
                Some(2),
                5002,
            ),
            (
                "duplicate compatibility details",
                r#","usage":{"input_tokens":5000,"output_tokens":2,"total_tokens":5002,"completion_tokens_details":null,"completion_tokens_details":{"reasoning_tokens":1}}"#,
                Some(5000),
                Some(2),
                5002,
            ),
            (
                "duplicate count",
                r#","usage":{"input_tokens":null,"input_\u0074okens":5000,"output_tokens":2,"total_tokens":5002}"#,
                None,
                None,
                0,
            ),
            (
                "duplicate usage",
                r#","usage":null,"usage":{"input_tokens":3,"output_tokens":2}"#,
                None,
                None,
                0,
            ),
            (
                "duplicate cache count",
                r#","usage":{"input_tokens":3,"output_tokens":2,"input_tokens_details":{"cached_tokens":0,"cached_tokens":1}}"#,
                None,
                None,
                0,
            ),
            (
                "above admission bounds",
                r#","usage":{"input_tokens":5000,"output_tokens":2,"total_tokens":5002}"#,
                Some(5000),
                Some(2),
                5002,
            ),
        ] {
            let terminal =
                format!("data: {{\"type\":\"{event}\",\"response\":{wire_snapshot}{usage}}}}}\n\n")
                    .into_bytes();
            let at = format!("{event} with {status} snapshot and {label} usage");
            let wire = [prefix.clone(), terminal, late_completion.clone()].concat();
            let reply = w.ask_hanging(Client::Responses, wire, &at).await;
            assert_eq!(reply.status, 200, "{at}");
            assert_eq!(last_event(&reply.body)["type"], "error", "{at}");
            assert_eq!(told(Client::Responses, &reply.body), class.code(), "{at}");
            assert_eq!(
                reply
                    .body
                    .lines()
                    .filter(|line| *line == "event: error")
                    .count(),
                1,
                "{at}"
            );
            assert!(
                reply.body.contains("Hello"),
                "{at}: a delta must reach the client"
            );
            assert!(!reply.body.contains("Different final text"), "{at}");

            for event in ["response.completed", "response.incomplete", "[DONE]"] {
                assert!(!reply.body.contains(event), "{at}: unexpected {event}");
            }
            let got = recorded(&w.f.store.pool, w.deployment, reply.id).await;
            let want = Recorded {
                input,
                output,
                ..failed_with(class, None)
            };
            if got != want {
                problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
            }
            type AccountingRow = (
                i64,
                i64,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                i64,
            );
            let (held, admitted, reserved_input, reserved_output, ledger_input, ledger_output, floor): AccountingRow = sqlx::query_as(
                "SELECT r.held_microusd,h.amount_microusd,r.input_tokens,r.output_tokens,u.input_tokens,u.output_tokens,u.amount_microusd
                 FROM inference_executions e
                 JOIN governance_reservations r ON r.execution_id=e.id AND r.admitted_at=e.started_at
                 JOIN monetary_ledger h ON h.execution_id=e.id AND h.admitted_at=e.started_at AND h.kind='hold'
                 JOIN monetary_ledger u ON u.execution_id=e.id AND u.admitted_at=e.started_at AND u.kind='unknown'
                 WHERE e.root_request_id=$1",
            )
            .bind(reply.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
            let expected_reasoning = match label {
                "conflicting reasoning fields" | "reasoning fits total less cache floor" => {
                    Some(Some(1))
                }
                "reasoning exceeds total less input"
                | "reasoning exceeds total less cache floor"
                | "duplicate reasoning count"
                | "duplicate compatibility details" => Some(None),
                _ => None,
            };
            if let Some(expected_reasoning) = expected_reasoning {
                let telemetry: (Option<i64>, Option<String>) = sqlx::query_as(
                    "SELECT reasoning_tokens,reported_upstream_model FROM inference_executions WHERE root_request_id=$1",
                )
                .bind(reply.id)
                .fetch_one(&w.f.store.pool)
                .await
                .unwrap();
                let want = (expected_reasoning, Some("served".into()));
                if telemetry != want {
                    problems.push(format!("{at}: telemetry {telemetry:?}, want {want:?}"));
                }
            }
            assert!(admitted > 0, "{at}: the fixture must reserve money");
            let got = (
                reserved_input,
                reserved_output,
                ledger_input,
                ledger_output,
                floor,
                held,
            );
            let want = (
                input,
                output,
                input,
                output,
                expected_floor,
                admitted.max(expected_floor),
            );
            if got != want {
                problems.push(format!(
                    "{at}: reservation and ledger {got:?}, want {want:?}"
                ));
            }
            if label == "above admission bounds" {
                assert!(
                    held > admitted,
                    "{at}: the known floor must increase the hold"
                );
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// #622: a reset or a body that ends before the terminal frame (including
/// inside a frame) ends the 200 as `upstream_unavailable` and count against
/// health; a whole frame with wrong content ends it as
/// `invalid_upstream_response` and does not.
async fn transport_failures(pool: PgPool, kind: Kind) {
    use InferenceError as E;
    let w = world(pool, kind, false).await;
    let (content, rest, _) = kind.valid();
    let cut = rest[..rest.len() / 2].to_vec();
    let cases: Vec<(&str, Vec<Step>, E)> = vec![
        (
            "reset after content",
            vec![Step::Bytes(content.clone()), Step::Reset],
            E::UpstreamUnavailable,
        ),
        (
            "reset inside a frame",
            vec![
                Step::Bytes([content.clone(), cut.clone()].concat()),
                Step::Reset,
            ],
            E::UpstreamUnavailable,
        ),
        (
            "end after content",
            vec![Step::Bytes(content.clone())],
            E::UpstreamUnavailable,
        ),
        (
            "end inside a frame",
            vec![Step::Bytes([content.clone(), cut].concat())],
            E::UpstreamUnavailable,
        ),
        (
            "end before the terminal frame",
            vec![Step::Bytes([content.clone(), rest].concat())],
            E::UpstreamUnavailable,
        ),
        (
            "bytes that are not UTF-8",
            vec![Step::Bytes(
                [content.clone(), b"data: \xff\xfe\n\n".to_vec()].concat(),
            )],
            E::InvalidUpstream,
        ),
        (
            "a frame that is not JSON",
            vec![Step::Bytes(
                [content.clone(), b"data: {not json\n\n".to_vec()].concat(),
            )],
            E::InvalidUpstream,
        ),
        (
            "a frame of the wrong shape",
            vec![Step::Bytes([content, kind.wrong_shape()].concat())],
            E::InvalidUpstream,
        ),
    ];
    let mut problems = Vec::new();
    for client in clients(kind) {
        for (place, steps, class) in &cases {
            let reply = w.ask(client, steps.clone()).await;
            let at = format!("{kind:?} {client:?} {place}");
            // The chat-shaped providers report usage before their terminal
            // frame: counts the stream did report stay recorded.
            let tokens = (*place == "end before the terminal frame"
                && matches!(kind, Kind::OpenAi | Kind::OpenRouter | Kind::Local))
            .then_some((3, 2));
            check(&w, client, &reply, *class, tokens, &at, &mut problems).await;
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// A connection of its own to the test database, outside the test pool.
async fn connect(pool: &PgPool) -> sqlx::postgres::PgConnection {
    <sqlx::postgres::PgConnection as sqlx::Connection>::connect_with(&pool.connect_options())
        .await
        .unwrap()
}
/// A health observation that outlives its 250 ms advisory timeout
/// (`ExecutionGuard::finish`) is abandoned by its request, but its single
/// statement is already on the server and commits after the reply ended. Each
/// case still records exactly its own observation: here the first write is
/// held by an advisory lock until its reply has ended (so the race is really
/// taken), then released, and must count for the first case, never for the
/// second.
#[dbtest::test]
async fn a_health_write_that_outlives_its_timeout_counts_for_its_own_case(pool: PgPool) {
    const HOLD: i64 = 828_828;
    // The lock is held on a connection of its own, outside the test pool the
    // barrier takes.
    let mut gate = connect(&pool).await;
    sqlx::raw_sql(
        // Only the first write waits for the lock; a sequence counts the
        // writes outside any transaction.
        "CREATE SEQUENCE health_hold_once;
         CREATE FUNCTION health_hold_once() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
          IF nextval('health_hold_once') = 1 THEN
           PERFORM pg_advisory_xact_lock(828828);
          END IF;
          RETURN NEW;
         END $$;
         CREATE TRIGGER health_hold_once BEFORE INSERT ON deployment_health
          FOR EACH ROW EXECUTE FUNCTION health_hold_once();",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(HOLD)
        .execute(&mut gate)
        .await
        .unwrap();
    let kind = Kind::Anthropic;
    let w = world(pool, kind, false).await;
    let (content, rest, _) = kind.valid();
    let cut = rest[..rest.len() / 2].to_vec();
    let mut problems = Vec::new();
    for (n, (place, steps)) in [
        (
            "reset after content",
            vec![Step::Bytes(content.clone()), Step::Reset],
        ),
        (
            "reset inside a frame",
            vec![Step::Bytes([content.clone(), cut].concat()), Step::Reset],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let reply = w.ask(Client::Messages, steps).await;
        if n == 0 {
            // The reply ended while its health write still waits for the lock.
            let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM deployment_health")
                .fetch_one(&mut gate)
                .await
                .unwrap();
            assert_eq!(seen, 0, "the held write landed before its reply ended");
            // Released only after this case has gone on to read its health:
            // the read must wait for the write, not find it already there.
            let mut held = std::mem::replace(&mut gate, connect(&w.f.store.pool).await);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                sqlx::query("SELECT pg_advisory_unlock($1)")
                    .bind(HOLD)
                    .execute(&mut held)
                    .await
                    .unwrap();
            });
        }
        let at = format!("{kind:?} held health {place}");
        check(
            &w,
            Client::Messages,
            &reply,
            InferenceError::UpstreamUnavailable,
            None,
            &at,
            &mut problems,
        )
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The decision's main promise: a stream that fails, even before any content,
/// is never retried or failed over, however permissive the route's policy.
/// Control: the same overload as an HTTP error before any stream does fail
/// over under that policy, so the policy is live.
async fn never_failed_over(pool: PgPool, kind: Kind) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let mut deployments = Vec::new();
    for (order, model) in [(0, "model-first"), (1, "model-second")] {
        deployments
            .push(add_deployment(&f, kind.provider(), kind.credential(), None, model, order).await);
    }
    only_deployments(&f, &deployments).await;
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,true)")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in deployments.iter().zip([0, 1]) {
        // Failover candidates must share an explicit residency with the first.
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    let (content, rest, terminal) = kind.valid();
    let good = [content.clone(), rest, terminal].concat();
    // (label, the first deployment's answer: an HTTP status or a stream)
    let first_answers: Vec<(&str, Option<u16>, Vec<Step>)> = vec![
        (
            "in-stream rate limit before content",
            None,
            vec![Step::Bytes(sse(&kind.errors("rate_limit_error")[..1]))],
        ),
        (
            "in-stream overload after content",
            None,
            vec![Step::Bytes(
                [content.clone(), sse(&kind.errors("overloaded_error")[..1])].concat(),
            )],
        ),
        (
            "in-stream server error after content",
            None,
            vec![Step::Bytes(
                [content.clone(), sse(&kind.errors("server_error")[..1])].concat(),
            )],
        ),
        (
            "reset after content",
            None,
            vec![Step::Bytes(content), Step::Reset],
        ),
        (
            "an HTTP 429 (control)",
            Some(429),
            vec![Step::Bytes(b"{}".to_vec())],
        ),
    ];
    for client in clients(kind) {
        for (label, status, first) in &first_answers {
            reset_health(&f.store.pool).await;
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let (log, first, good, status) = (seen.clone(), first.clone(), good.clone(), *status);
            let app = Router::new().fallback(move |body: Bytes| {
                let (log, first, good) = (log.clone(), first.clone(), good.clone());
                async move {
                    let model = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|v| v["model"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    log.lock().unwrap().push(model.clone());
                    if model != "model-first" {
                        return response(200, "text/event-stream", vec![Step::Bytes(good)]);
                    }
                    match status {
                        Some(code) => response(code, "application/json", first),
                        None => response(200, "text/event-stream", first),
                    }
                }
            });
            let (mock_base, _mock) = serve(app).await;
            let (base, _gateway) = register(&f, kind, &mock_base).await;
            let before: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let reply = call(&base, &token, client).await;
            let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let models = seen.lock().unwrap().clone();
            let at = format!("{kind:?} {client:?} {label}");
            assert_eq!(reply.status, 200, "{at}: {}", reply.body);
            if status.is_some() {
                // Before any stream the same overload fails over.
                assert_eq!(models, ["model-first", "model-second"], "{at}");
                assert_eq!(attempts - before, 2, "{at}");
                assert!(
                    reply.body.contains("[DONE]")
                        || reply.body.contains("message_stop")
                        || reply.body.contains("response.completed"),
                    "{at}: {}",
                    reply.body
                );
            } else {
                assert_eq!(
                    models,
                    ["model-first"],
                    "{at}: only the first deployment is called"
                );
                assert_eq!(attempts - before, 1, "{at}");
                assert!(
                    !reply.body.contains("[DONE]") && !reply.body.contains("message_stop"),
                    "{at}"
                );
            }
        }
    }
}

/// Routing health follows the class: three in-stream overloads, server errors
/// or resets open the breaker (and the next request is refused as cooling
/// down); three client-fault errors or three invalid responses do not.
async fn breaker(pool: PgPool, kind: Kind) {
    let w = world(pool, kind, false).await;
    let (content, _, _) = kind.valid();
    let client = clients(kind)[0];
    let frame = |name: &str| sse(&kind.errors(name)[..1]);
    let cases: Vec<(&str, Vec<Step>, bool)> = vec![
        (
            "an in-stream overload",
            vec![Step::Bytes(
                [content.clone(), frame("server_error")].concat(),
            )],
            true,
        ),
        (
            "an in-stream rate limit",
            vec![Step::Bytes(
                [content.clone(), frame("rate_limit_error")].concat(),
            )],
            true,
        ),
        (
            "a reset",
            vec![Step::Bytes(content.clone()), Step::Reset],
            true,
        ),
        (
            "a stream cut before its terminal frame",
            vec![Step::Bytes(content.clone())],
            true,
        ),
        (
            "an in-stream client-fault error",
            vec![Step::Bytes(
                [content.clone(), frame("invalid_request_error")].concat(),
            )],
            false,
        ),
        (
            "an in-stream configuration error",
            vec![Step::Bytes(
                [content.clone(), frame("authentication_error")].concat(),
            )],
            false,
        ),
        (
            "an invalid response",
            vec![Step::Bytes([content, kind.wrong_shape()].concat())],
            false,
        ),
    ];
    for (label, steps, opens) in cases {
        reset_health(&w.f.store.pool).await;
        *w.script.lock().unwrap() = steps;
        for n in 1..=3 {
            let reply = call(&w.base, &w.token, client).await;
            assert_eq!(reply.status, 200, "{kind:?} {label} #{n}: {}", reply.body);
        }
        health_writes_settled(&w.f.store.pool).await;
        let (failures, open): (i32, bool) = sqlx::query_as(
            "SELECT COALESCE(consecutive_failures,0), COALESCE(open_until>clock_timestamp(),false) FROM (SELECT 1) one LEFT JOIN deployment_health ON deployment_id=$1",
        )
        .bind(w.deployment)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            (failures, open),
            (if opens { 3 } else { 0 }, opens),
            "{kind:?} {label}"
        );
        if opens {
            // The route is cooling down: refused before dispatch.
            let reply = call(&w.base, &w.token, client).await;
            assert_ne!(reply.status, 200, "{kind:?} {label}: {}", reply.body);
            assert!(
                reply.body.contains("model_temporarily_unavailable"),
                "{kind:?} {label}: {}",
                reply.body
            );
        }
    }
}

/// A client that goes away mid-stream cancels the attempt; that is the
/// gateway's side, not the provider's: no failure and no success is recorded
/// against the route's health, whatever the provider was doing.
async fn client_drop(pool: PgPool, kind: Kind) {
    let w = world(pool, kind, false).await;
    let (content, _, _) = kind.valid();
    for client in clients(kind) {
        reset_health(&w.f.store.pool).await;
        let dropped = Arc::new(Notify::new());
        *w.script.lock().unwrap() = vec![Step::BytesThenHang(content.clone(), dropped.clone())];
        let mut response = client_request(&w.base, client)
            .bearer_auth(&w.token)
            .send()
            .await
            .unwrap();
        let id: Uuid = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        // Wait for actual upstream content before disconnecting the client.
        let at = format!("{kind:?} {client:?}");
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.unwrap() {
                body.extend_from_slice(&chunk);
                if body.windows(5).any(|bytes| bytes == b"Hello") {
                    return;
                }
            }
            panic!("{at}: no upstream content before EOF");
        })
        .await
        .unwrap_or_else(|_| panic!("{at}: no upstream content"));
        drop(response);
        tokio::time::timeout(Duration::from_secs(5), dropped.notified())
            .await
            .unwrap_or_else(|_| panic!("{at}: upstream body stayed open after disconnect"));
        let mut state = String::new();
        for _ in 0..100 {
            state = sqlx::query_scalar(
                "SELECT state FROM inference_executions WHERE root_request_id=$1",
            )
            .bind(id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
            if state != "started" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state, "cancelled", "{at}");
        // Give a wrongly recorded health observation time to start, then to land.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        health_writes_settled(&w.f.store.pool).await;
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM deployment_health WHERE deployment_id=$1")
                .bind(w.deployment)
                .fetch_one(&w.f.store.pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "{at}: a cancelled attempt left a health observation"
        );
    }
}

mod envelope;
