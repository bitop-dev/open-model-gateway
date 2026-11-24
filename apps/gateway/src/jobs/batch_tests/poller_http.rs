//! #802 on the real path short of a launched binary: the real poller loop
//! and the real OpenAI adapter over HTTP to a mock OpenAI batch API, on
//! PostgreSQL. One batch's result download stalls (its HTTP body never
//! ends) for many ticks; another batch's status refresh and a third batch's
//! cancel go through meanwhile, and stopping the poller closes the stalled
//! download's connection.
use std::{
    collections::HashMap,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::Response,
    routing::{get, post},
};

use super::*;
use crate::providers::stream_class_tests::Resolver;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Completed; its output file's download stalls.
    Stalls,
    InProgress,
}

#[derive(Default)]
struct Upstream {
    next: AtomicUsize,
    plans: StdMutex<HashMap<String, Plan>>,
    cancelled: StdMutex<Vec<String>>,
    refreshed: StdMutex<Vec<String>>,
    download_started: tokio::sync::Notify,
    download_dropped: tokio::sync::Notify,
}

fn batch_json(id: &str, status: &str, output: Option<&str>) -> Value {
    json!({"id":id,"object":"batch","endpoint":"/v1/chat/completions","model":"gpt-x",
        "input_file_id":"file-in","completion_window":"24h","status":status,
        "output_file_id":output,"error_file_id":null,"created_at":1791432289,
        "request_counts":{"total":1,"completed":0,"failed":0}})
}
fn json_response(value: Value) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn mock(state: Arc<Upstream>) -> Router {
    Router::new()
        .route(
            "/v1/files",
            // The upload is read whole before the answer, as a provider does.
            post(|body: axum::body::Bytes| async move {
                json_response(
                    json!({"id":"file-in","object":"file","purpose":"batch","bytes":body.len()}),
                )
            }),
        )
        .route(
            "/v1/batches",
            post(|State(s): State<Arc<Upstream>>| async move {
                let n = s.next.fetch_add(1, Ordering::SeqCst);
                json_response(batch_json(&format!("batch_{n}"), "validating", None))
            }),
        )
        .route(
            "/v1/batches/{id}",
            get(
                |State(s): State<Arc<Upstream>>, Path(id): Path<String>| async move {
                    s.refreshed.lock().unwrap().push(id.clone());
                    let plan = s.plans.lock().unwrap().get(&id).copied();
                    match plan {
                        Some(Plan::Stalls) => {
                            json_response(batch_json(&id, "completed", Some("file-out-stalls")))
                        }
                        _ => json_response(batch_json(&id, "in_progress", None)),
                    }
                },
            ),
        )
        .route(
            "/v1/batches/{id}/cancel",
            post(
                |State(s): State<Arc<Upstream>>, Path(id): Path<String>| async move {
                    s.cancelled.lock().unwrap().push(id.clone());
                    json_response(batch_json(&id, "cancelling", None))
                },
            ),
        )
        .route(
            "/v1/files/{id}/content",
            get(|State(s): State<Arc<Upstream>>| async move {
                // A body that starts and never ends; dropping it (the client
                // closed the connection) is signalled.
                struct Dropped(Arc<Upstream>);
                impl Drop for Dropped {
                    fn drop(&mut self) {
                        self.0.download_dropped.notify_one();
                    }
                }
                s.download_started.notify_one();
                let guard = Dropped(s.clone());
                let body = async_stream::stream! {
                    let _guard = guard;
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b""));
                    std::future::pending::<()>().await;
                };
                Response::builder()
                    .header("content-type", "application/jsonl")
                    .body(Body::from_stream(body))
                    .unwrap()
            }),
        )
        .with_state(state)
}

async fn until(what: &str, deadline: Duration, mut check: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !check().await {
        assert!(
            start.elapsed() < deadline,
            "{what} did not happen in {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[dbtest::test]
async fn over_http_a_stalled_download_does_not_delay_other_jobs(pool: PgPool) {
    let upstream = Arc::new(Upstream::default());
    let (base, _server) =
        crate::inference::admission_validation_tests::serve(mock(upstream.clone())).await;
    let adapter = Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
        Arc::new(Resolver),
        format!("{base}/v1"),
    ));
    let mut w = world_with(pool, false, false, 1, Some(adapter)).await;
    v3_price(&w.f, w.f.deployment, true).await;
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,10) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_jobs=10")
        .bind(w.f.principal.workspace_id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let mut jobs = Vec::new();
    for name in ["stalls", "refreshes", "cancels"] {
        let file = w
            .upload(&w.f.principal, &[chat_line(name, "company/smart", name)])
            .await;
        jobs.push(
            w.create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
                .await
                .unwrap(),
        );
    }
    w.poll().await;
    let mut rows = Vec::new();
    for job in &jobs {
        let row = w.reload(job).await;
        assert_eq!(row.mode(), Some(BatchMode::Native));
        rows.push(row);
    }
    let id = |row: &JobRow| row.upstream_id.clone().expect("submitted");
    let (stalls, refreshes, cancels) = (&rows[0], &rows[1], &rows[2]);
    {
        let mut plans = upstream.plans.lock().unwrap();
        plans.insert(id(stalls), Plan::Stalls);
        plans.insert(id(refreshes), Plan::InProgress);
        plans.insert(id(cancels), Plan::InProgress);
    }
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id<>$1")
        .bind(stalls.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()-interval '1 minute' WHERE id=$1")
        .bind(stalls.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        upstream.download_started.notified(),
    )
    .await
    .expect("the stalled download started");
    sqlx::query("UPDATE async_jobs SET cancel_requested_at=now(),next_poll_at=now() WHERE id=$1")
        .bind(cancels.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    until("the other refresh and the cancel", tick * 50, async || {
        w.reload(refreshes).await.upstream_status.as_deref() == Some("in_progress")
            && upstream.cancelled.lock().unwrap().contains(&id(cancels))
    })
    .await;
    // Several ticks later the other job has been refreshed again while the
    // download is still open.
    let seen = |s: &Upstream| {
        s.refreshed
            .lock()
            .unwrap()
            .iter()
            .filter(|i| **i == id(refreshes))
            .count()
    };
    let before = seen(&upstream);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(refreshes.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    until("a second refresh", tick * 50, async || {
        seen(&upstream) > before
    })
    .await;
    let blocked = w.reload(stalls).await;
    assert_eq!(blocked.upstream_status.as_deref(), Some("finalizing"));
    assert!(blocked.settled_at.is_none());
    // Stopping the poller closes the stalled download's connection.
    poller.abort();
    tokio::time::timeout(
        Duration::from_secs(10),
        upstream.download_dropped.notified(),
    )
    .await
    .expect("the stalled download's connection was closed");
    let after = w.reload(stalls).await;
    assert!(after.settled_at.is_none() && after.output_file_id.is_none());
    consistent(&w.f).await;
}
