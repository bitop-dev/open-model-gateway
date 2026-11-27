//! The structure bound on JSON bodies (#872) through the real router, on
//! every JSON inference route: a hostile body is refused with 413 in its
//! route's shape before any parse, nothing is admitted or sent, and the
//! refusal is counted by route family; legitimate large requests are
//! served. Also the per-key share of the image input buffer.
use super::*;
use crate::{
    inference::{image_buffer::TEXT_BODY_BYTES, json_weight},
    metrics::METRICS,
};

/// A body of at most `bytes` made of `item` repeated in an array under `x`,
/// beside a valid model (each route then refuses it on its own grammar if
/// it reaches the parser, which it must not).
fn hostile(item: &str, bytes: usize) -> Vec<u8> {
    let head = r#"{"model":"company/smart","x":["#;
    let n = (bytes - head.len() - 2) / (item.len() + 1);
    format!("{head}{}]}}", vec![item; n].join(",")).into_bytes()
}

const ROUTES: [(&str, &str); 11] = [
    ("/v1/chat/completions", "chat_completions"),
    ("/v1/responses", "responses"),
    ("/v1/messages", "messages"),
    ("/v1/messages/count_tokens", "messages"),
    ("/v1/embeddings", "embeddings"),
    ("/v1/rerank", "rerank"),
    ("/v1/systemone", "systemone"),
    ("/v1/images/generations", "images"),
    ("/v1/audio/speech", "audio"),
    ("/v1/videos", "videos"),
    ("/v1/batches", "batches"),
];

#[dbtest::test]
async fn a_body_that_parses_too_large_is_refused_before_any_parse_on_every_json_route(
    pool: PgPool,
) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    allow_images(&w, true).await;
    // One-key objects (a B-tree node each) and numbers (a value each), just
    // under the 2 MiB text limit.
    for item in [r#"{"a":0}"#, "0"] {
        let body = hostile(item, TEXT_BODY_BYTES);
        assert!(json_weight::estimate(&body) > json_weight::BUDGET);
        for (path, family) in ROUTES {
            for declared in [true, false] {
                let before = METRICS.json_structure_refusal_count(family);
                let (status, _, value, _) =
                    send(&w, path, Some(&w.token), body.clone(), declared).await;
                assert_eq!(
                    status,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "{path} {item}: {value}"
                );
                if path.starts_with("/v1/messages") {
                    assert_eq!(value["error"]["type"], "request_too_large", "{path}");
                } else {
                    assert_eq!(value["error"]["code"], "payload_too_large", "{path}");
                }
                assert!(!value.to_string().contains(item), "{path}: no echo");
                assert_eq!(
                    METRICS.json_structure_refusal_count(family),
                    before + 1,
                    "{path}"
                );
            }
        }
    }
    // A large image body whose text is within 2 MiB but would parse into too
    // much structure: refused by the same pass that measures its images.
    let url = png_url(3 * MIB, "s");
    let numbers = vec!["0"; 900_000].join(",");
    let body = format!(
        r#"{{"model":"company/smart","max_tokens":16,"messages":[{{"role":"user","content":[{{"type":"image_url","image_url":{{"url":"{url}"}}}}]}}],"tool_choice":[{numbers}]}}"#
    );
    let before = METRICS.json_structure_refusal_count("chat_completions");
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        body.into_bytes(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
    assert_eq!(
        METRICS.json_structure_refusal_count("chat_completions"),
        before + 1
    );
    assert_eq!(executions(&w).await, 0, "nothing admitted");
    assert!(w.upstream.bodies.lock().unwrap().is_empty(), "nothing sent");
}

/// A tool definition of `props` properties, each with a description of
/// `chars` characters, in Chat (`function.parameters`) or Messages
/// (`input_schema`) form.
fn tool(i: usize, props: usize, chars: usize, messages: bool) -> Value {
    let schema = json!({
        "type": "object",
        "properties": (0..props)
            .map(|p| (format!("p{p}"), json!({"type": "string", "description": "d".repeat(chars)})))
            .collect::<serde_json::Map<_, _>>(),
        "required": (0..props).map(|p| format!("p{p}")).collect::<Vec<_>>(),
    });
    if messages {
        json!({"name": format!("tool_{i}"), "description": "t".repeat(200), "input_schema": schema})
    } else {
        json!({"type": "function", "function": {"name": format!("tool_{i}"), "description": "t".repeat(200), "parameters": schema}})
    }
}

#[dbtest::test]
async fn legitimate_large_requests_are_served(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let text = "word ".repeat((TEXT_BODY_BYTES - 1024) / 5);
    // As many tools as the grammars allow, each with a described schema,
    // and one tool whose schema alone is large.
    let chat_tools: Vec<Value> = (0..128).map(|i| tool(i, 12, 80, false)).collect();
    let messages_tools: Vec<Value> = (0..128).map(|i| tool(i, 12, 80, true)).collect();
    let large_schema = tool(0, 5000, 80, true);
    // A long agentic conversation: tool calls and their results.
    let mut turns = vec![json!({"role":"user","content":"start"})];
    for i in 0..1500 {
        turns.push(json!({"role":"assistant","content":[{"type":"tool_use","id":format!("c{i}"),"name":"read","input":{"path":format!("src/file_{i}.rs"),"lines":[1, 200]}}]}));
        turns.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":format!("c{i}"),"content":"x".repeat(600)}]}));
    }
    turns.push(json!({"role":"user","content":"done?"}));
    let bodies = [
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":text}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"字".repeat((TEXT_BODY_BYTES - 1024) / 3)}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"tools":chat_tools}),
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","max_output_tokens":16,"input":"hi","tools":chat_tools.iter().map(|t| {
                let mut f = t["function"].clone();
                f["type"] = json!("function");
                f
            }).collect::<Vec<_>>()}),
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"tools":messages_tools}),
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"tools":[large_schema]}),
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":16,"messages":turns,"tools":[tool(0, 5, 80, true)]}),
        ),
    ];
    for (path, body) in bodies {
        let body = serde_json::to_vec(&body).unwrap();
        assert!(body.len() <= TEXT_BODY_BYTES, "{path}: {}", body.len());
        let estimate = json_weight::estimate(&body);
        assert!(estimate <= json_weight::BUDGET, "{path}: {estimate}");
        let (status, _, value, _) = send(&w, path, Some(&w.token), body, true).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
    }
}

/// One key holds at most half the image input buffer, across its count and
/// generation requests: at its share that key gets 503 while another key's
/// large body is admitted, and the share comes back when a body's permit
/// is released, a dropped connection included.
#[dbtest::test]
async fn one_key_holds_at_most_half_the_image_buffer(pool: PgPool) {
    let limits = ImageInputLimits {
        body_bytes: 4 * MIB,
        buffer_bytes: 12 * MIB,
        ..ImageInputLimits::default()
    };
    let mut w = world(pool, limits, true).await;
    allow_images(&w, true).await;
    let first = w.token.clone();
    let second = issue_key(&mut w.f).await;
    let count = |reason: &str| METRICS.image_input_refusal_count(reason);
    let in_use = || METRICS.image_input_buffer().0.get();
    // A large body that takes exactly 3 MiB of the buffer: two of them are
    // the key's whole share (6 of 12 MiB).
    let body = serde_json::to_vec(&chat_body(&png_url(2 * MIB + 128 * 1024, "k"), false)).unwrap();
    assert!(
        body.len() > 2 * MIB && body.len() <= 3 * MIB,
        "{}",
        body.len()
    );
    // A large body that sends half of itself and waits. The server reads a
    // body only once it holds its share, so the first read is the signal
    // that the share is taken.
    let held = |path: &'static str, token: String| {
        let head = Bytes::copy_from_slice(&body[..body.len() / 2]);
        let (read, admitted) = tokio::sync::oneshot::channel::<()>();
        let stalled = Body::from_stream(
            futures_util::stream::once(async move {
                let _ = read.send(());
                Ok::<_, std::convert::Infallible>(head)
            })
            .chain(futures_util::stream::pending()),
        );
        let task = tokio::spawn(
            w.router.clone().oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::CONTENT_LENGTH, body.len())
                    .header("anthropic-version", "2023-06-01")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(stalled)
                    .unwrap(),
            ),
        );
        (task, admitted)
    };
    let before = in_use();
    let (generation, generation_admitted) = held("/v1/chat/completions", first.clone());
    generation_admitted
        .await
        .expect("the generation is admitted");
    let (counting, counting_admitted) = held("/v1/messages/count_tokens", first.clone());
    counting_admitted
        .await
        .expect("the count is admitted beside it");
    assert_eq!(in_use() - before, 6 * MIB as i64, "both hold their share");
    // The first key holds 6 of 12 MiB: its share.
    let shares = count("key_share");
    let (status, retry, value, pulled) =
        send(&w, "/v1/chat/completions", Some(&first), body.clone(), true).await;
    assert_eq!(
        (status, retry.as_deref(), pulled),
        (StatusCode::SERVICE_UNAVAILABLE, Some("1"), 0),
        "{value}"
    );
    assert_eq!(value["error"]["code"], "image_input_busy");
    assert_eq!(count("key_share"), shares + 1);
    // Another key's large body is admitted and served meanwhile.
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&second),
        body.clone(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    // The client goes away: once its request is dropped (the join below
    // returns after the task's future is dropped), its share is back, and
    // the first key may send again.
    generation.abort();
    assert!(generation.await.unwrap_err().is_cancelled());
    assert_eq!(
        in_use() - before,
        3 * MIB as i64,
        "the dropped body's share is back"
    );
    let (status, _, value, _) =
        send(&w, "/v1/chat/completions", Some(&first), body.clone(), true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    counting.abort();
    assert!(counting.await.unwrap_err().is_cancelled());
    assert_eq!(in_use(), before, "every share is back");
}

/// A body without a declared length is read only up to its route's cap:
/// past it the request is refused with 413, as before the structure bound,
/// with no more of the body pulled than the cap and one frame.
#[dbtest::test]
async fn an_undeclared_body_past_its_route_cap_is_read_only_to_the_cap(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let text = format!(
        r#"{{"model":"company/smart","input":"{}"}}"#,
        "x".repeat(3 * MIB)
    );
    for path in [
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/systemone",
        "/v1/images/generations",
        "/v1/audio/speech",
        "/v1/batches",
        "/v1/chat/completions",
        "/v1/messages",
    ] {
        let (status, _, value, pulled) =
            send(&w, path, Some(&w.token), text.clone().into_bytes(), false).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}: {value}");
        assert!(
            pulled <= TEXT_BODY_BYTES + 64 * 1024,
            "{path}: pulled {pulled}"
        );
    }
}

/// A body is measured whatever its `Content-Type` claims only on the route
/// whose handler reads a loosely typed body as JSON (`/v1/videos` compares
/// only the media type before the first `;`). Every other JSON route refuses
/// a body that is not JSON by its type unread, as before the bound.
#[dbtest::test]
async fn only_the_videos_route_measures_a_body_whatever_its_content_type(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let body = hostile(r#"{"a":0}"#, TEXT_BODY_BYTES);
    for (path, content_type, measured) in [
        ("/v1/videos", Some("application/json; charset"), true),
        ("/v1/videos", Some("application/json;=x"), true),
        ("/v1/rerank", Some("text/plain"), false),
        ("/v1/embeddings", Some("application/json; charset"), false),
        (
            "/v1/chat/completions",
            Some("application/json; charset"),
            false,
        ),
        ("/v1/messages", None, false),
    ] {
        let pulled = Arc::new(AtomicUsize::new(0));
        let mut request = Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
            .header("anthropic-version", "2023-06-01");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let response = w
            .router
            .clone()
            .oneshot(request.body(counted(body.clone(), pulled.clone())).unwrap())
            .await
            .unwrap();
        if measured {
            assert_eq!(
                response.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "{path} {content_type:?}"
            );
        } else {
            assert_ne!(
                response.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "{path} {content_type:?}"
            );
            assert!(
                response.status().is_client_error(),
                "{path} {content_type:?}"
            );
            assert_eq!(
                pulled.load(Ordering::SeqCst),
                0,
                "{path} {content_type:?}: unread"
            );
        }
    }
}

/// The measure wraps only the handlers that read a JSON body, inside
/// authentication: a method no handler takes is answered 405 with no body
/// byte read, authenticated or not, and a GET with a body does not have it
/// read.
#[dbtest::test]
async fn a_body_is_read_only_for_a_handler_that_reads_it(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let body = hostile(r#"{"a":0}"#, TEXT_BODY_BYTES);
    let send_with = |method: &str, path: &str, token: Option<&str>| {
        let pulled = Arc::new(AtomicUsize::new(0));
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request.body(counted(body.clone(), pulled.clone())).unwrap();
        let router = w.router.clone();
        async move {
            (
                router.oneshot(request).await.unwrap().status(),
                pulled.load(Ordering::SeqCst),
            )
        }
    };
    for path in [
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/systemone",
        "/v1/images/generations",
        "/v1/audio/speech",
        "/v1/videos",
        "/v1/batches",
    ] {
        // Without a key, the route answers 401 (where authentication wraps
        // every method) or 405; with one, 405. No body byte is read.
        let (status, pulled) = send_with("PUT", path, None).await;
        assert!(
            [StatusCode::UNAUTHORIZED, StatusCode::METHOD_NOT_ALLOWED].contains(&status),
            "{path}: {status}"
        );
        assert_eq!(pulled, 0, "{path}: no body byte read without a key");
        let (status, pulled) = send_with("PUT", path, Some(w.token.as_str())).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(pulled, 0, "{path}: no body byte read for a 405");
    }
    for path in ["/v1/batches", "/v1/videos"] {
        let (status, pulled) = send_with("GET", path, Some(&w.token)).await;
        assert_ne!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert_eq!(pulled, 0, "{path}: a listing reads no body");
    }
}

/// An image written with escaped slashes (as some encoders write every
/// `/`), near the per-image limit, beside a full set of described tools, is
/// served: the parser's scratch copy of the image is the image input
/// buffer's, so only the tools count as structure. Charged as structure, the
/// same body would be over the budget.
#[dbtest::test]
async fn an_escaped_image_near_its_limit_is_served(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    allow_images(&w, true).await;
    let mut body = chat_body(&png_url(5 * MIB - 1024, "s"), false);
    body["tools"] = json!((0..128).map(|i| tool(i, 24, 80, false)).collect::<Vec<_>>());
    let escaped = serde_json::to_string(&body).unwrap().replace('/', r"\/");
    assert!(
        escaped.contains(r"data:image\/png"),
        "the image's slashes are escaped"
    );
    assert!(
        json_weight::estimate(escaped.as_bytes()) > json_weight::BUDGET,
        "charged as structure, the image's scratch copy would refuse it"
    );
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        escaped.into_bytes(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
}

/// A multipart form is not measured: it reaches its handler, which reads it
/// as the form it is, whatever its parts hold.
#[dbtest::test]
async fn a_multipart_form_is_not_measured(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let objects = vec![r#"{"a":0}"#; 200_000].join(",");
    let form = format!(
        "--x\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n[{objects}]\r\n--x--\r\n"
    );
    assert!(json_weight::estimate(form.as_bytes()) > json_weight::BUDGET);
    for content_type in [
        "multipart/form-data; boundary=x",
        " Multipart/Form-Data; boundary=x",
    ] {
        let before = METRICS.json_structure_refusal_count("videos");
        let response = w
            .router
            .clone()
            .oneshot(
                Request::post("/v1/videos")
                    .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(form.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "{content_type}"
        );
        assert_eq!(
            METRICS.json_structure_refusal_count("videos"),
            before,
            "{content_type}"
        );
    }
}
