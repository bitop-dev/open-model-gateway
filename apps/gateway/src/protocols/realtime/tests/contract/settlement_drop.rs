//! Abort the session owner while PostgreSQL is blocking its write, then read
//! what the detached writes leave: the session and response rows, the ledger,
//! the maintained budget totals and the engine's concurrency permit.
use super::*;
use crate::inference::{
    error::InferenceError,
    realtime::{RealtimeSession, RealtimeUsage, ResponseStatus},
    repository::Outcome,
};

/// usage(3 text in, 1 text out) under the test prices:
/// 3×4 + 1×16 = 28 µUSD.
const ACTUAL: i64 = 28;
fn usage() -> Option<RealtimeUsage> {
    Some(RealtimeUsage {
        input_text_tokens: 3,
        output_text_tokens: 1,
        ..Default::default()
    })
}
const COMPLETED: Option<ResponseStatus> = Some(ResponseStatus::Completed);

/// `(settled, held, pending, unknown)` of every lifetime budget total (the
/// workspace's and the key lineage's): one session is the only reservation.
async fn totals(g: &Gateway) -> Vec<(i64, i64, i64, i64)> {
    sqlx::query_as("SELECT settled_microusd::bigint,held_microusd::bigint,pending,unknown FROM budget_totals WHERE period='lifetime' ORDER BY scope_kind")
        .fetch_all(&g.pool)
        .await
        .unwrap()
}
async fn open(g: &Gateway) -> (RealtimeSession, Uuid) {
    let principal = Store::new(g.pool.clone())
        .authenticate(g.key())
        .await
        .unwrap()
        .unwrap();
    let id = Uuid::now_v7();
    let session = g
        .engine
        .open_realtime(principal, "company/smart", id)
        .await
        .unwrap();
    (session, id)
}
/// A transaction holding the session's reservation row, and its backend pid.
async fn lock(g: &Gateway, id: Uuid) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut blocker = g.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM governance_reservations WHERE execution_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    (blocker, pid)
}
/// Backends of this database waiting on any of `pids`. A row lock's queue
/// reports a later waiter as blocked by the waiter ahead of it, not by the
/// holder.
async fn waiting_on(g: &Gateway, pids: &[i32]) -> Vec<i32> {
    sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND pg_blocking_pids(pid) && $1::int[] ORDER BY pid")
        .bind(pids)
        .fetch_all(&g.pool)
        .await
        .unwrap()
}
/// The first backend waiting on any of `pids` other than `except` (bounded
/// wait).
async fn waiter(g: &Gateway, pids: &[i32], except: Option<i32>) -> i32 {
    tokio::time::timeout(SETTLE_BOUND, async {
        loop {
            if let Some(p) = waiting_on(g, pids)
                .await
                .into_iter()
                .find(|p| Some(*p) != except)
            {
                return p;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("a write reached the locked reservation")
}
/// The engine's permits return to `full` once the detached writes end.
async fn permits_return(g: &Gateway, full: usize) {
    tokio::time::timeout(SETTLE_BOUND, async {
        while g.engine.available_capacity() != full {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the detached write gave its concurrency permit back");
}
fn settlements(ledger: &[(String, Option<i64>)]) -> Vec<&(String, Option<i64>)> {
    ledger
        .iter()
        .filter(|(kind, _)| kind == "settlement" || kind == "unknown")
        .collect()
}

async fn interrupted(pool: PgPool, during_response: bool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let full = g.engine.available_capacity();
    let (mut session, id) = open(&g).await;
    assert_eq!(g.engine.available_capacity(), full - 1);
    session.reserve_window(100).await.unwrap();
    session.open_response("one".into()).await.unwrap();
    if !during_response {
        session
            .settle_response("one", COMPLETED, usage())
            .await
            .unwrap();
    }

    let (blocker, pid) = lock(&g, id).await;
    let owner = tokio::spawn(async move {
        if during_response {
            session
                .settle_response("one", COMPLETED, usage())
                .await
                .unwrap();
        } else {
            session
                .finish(Outcome::Succeeded, None, None)
                .await
                .unwrap();
        }
    });
    waiter(&g, &[pid], None).await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    // The detached write still holds the session's permit.
    assert_eq!(g.engine.available_capacity(), full - 1);
    blocker.rollback().await.unwrap();

    let row = tokio::time::timeout(Duration::from_secs(5), g.finished())
        .await
        .expect("session settlement completed");
    assert_eq!(
        row["state"],
        if during_response {
            "cancelled"
        } else {
            "succeeded"
        }
    );
    assert_eq!(row["reservation"], "settled");
    assert_eq!(row["actual"], ACTUAL);
    assert_eq!(row["input_tokens"], 3);
    assert_eq!(row["output_tokens"], 1);
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["status"], "completed");
    assert_eq!(responses[0]["actual"], ACTUAL);
    let ledger = g.ledger(&id.to_string()).await;
    assert_eq!(
        settlements(&ledger),
        vec![&("settlement".into(), Some(ACTUAL))]
    );
    // Applied once, with no hold left behind.
    assert_eq!(totals(&g).await, vec![(ACTUAL, 0, 0, 0); 2]);
    permits_return(&g, full).await;
}

#[dbtest::test]
async fn drop_during_final_write(pool: PgPool) {
    interrupted(pool, false).await;
}

#[dbtest::test]
async fn drop_during_response_write(pool: PgPool) {
    interrupted(pool, true).await;
}

/// Response one settled, response two still open when the owner is aborted:
/// the session ends `unknown`, holding response two's window plus response
/// one's actual, with no actual of its own.
#[dbtest::test]
async fn drop_with_a_settled_response_and_an_open_one(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let full = g.engine.available_capacity();
    let (mut session, id) = open(&g).await;
    session.reserve_window(100).await.unwrap();
    session.open_response("one".into()).await.unwrap();
    session
        .settle_response("one", COMPLETED, usage())
        .await
        .unwrap();
    session.reserve_window(100).await.unwrap();
    session.open_response("two".into()).await.unwrap();
    let owner = tokio::spawn(async move {
        let _session = session;
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());

    let row = g.finished().await;
    assert_eq!(row["state"], "cancelled");
    assert_eq!(row["reservation"], "unknown");
    assert_eq!(row["actual"], Value::Null);
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses.len(), 2);
    assert_eq!(
        (&responses[0]["state"], &responses[0]["actual"]),
        (&json!("settled"), &json!(ACTUAL))
    );
    assert_eq!(
        (&responses[1]["state"], &responses[1]["actual"]),
        (&json!("unknown"), &Value::Null)
    );
    let window = responses[1]["window"].as_i64().unwrap();
    assert!(window > 0);
    assert_eq!(row["held"], window + ACTUAL);
    // The unknown entry's amount is the known floor: response one's actual.
    let ledger = g.ledger(&id.to_string()).await;
    assert_eq!(
        settlements(&ledger),
        vec![&("unknown".into(), Some(ACTUAL))]
    );
    assert_eq!(totals(&g).await, vec![(0, window + ACTUAL, 0, 1); 2]);
    permits_return(&g, full).await;
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    /// The response write's connection is terminated while it waits.
    Error,
    /// The response write waits past its settlement deadline.
    Deadline,
}

/// The owner is aborted while its response write waits on the locked
/// reservation, and that write then fails. The final write starts only once
/// the response write has ended, then finishes the session with the
/// response's window retained as unknown cost.
async fn failed_response_write(pool: PgPool, failure: Failure) {
    // Ten connections give the settlement gate three permits, so the gate
    // does not serialize the two writes: only the final write's wait does.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let full = g.engine.available_capacity();
    let (mut session, id) = open(&g).await;
    session.reserve_window(100).await.unwrap();
    session.open_response("one".into()).await.unwrap();

    let (blocker, pid) = lock(&g, id).await;
    let owner = tokio::spawn(async move {
        let _ = session.settle_response("one", COMPLETED, usage()).await;
    });
    let response_write = waiter(&g, &[pid], None).await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    match failure {
        Failure::Error => {
            // While the response write is in flight the final write has not
            // started: nothing else queues on the row and the session is open.
            // (A final write that did not wait would queue within moments.)
            let until = tokio::time::Instant::now() + Duration::from_millis(500);
            while tokio::time::Instant::now() < until {
                assert_eq!(
                    waiting_on(&g, &[pid, response_write]).await,
                    vec![response_write]
                );
                tokio::time::sleep(POLL).await;
            }
            let state: String =
                sqlx::query_scalar("SELECT state FROM inference_executions WHERE id=$1")
                    .bind(id)
                    .fetch_one(&g.pool)
                    .await
                    .unwrap();
            assert_eq!(state, "started");
            let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                .bind(response_write)
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert!(terminated);
            // The failed write lets the final write start.
            waiter(&g, &[pid], Some(response_write)).await;
        }
        Failure::Deadline => {
            // The final write queues only after the response write's
            // deadline, which leaves its backend queued ahead of it; end that
            // backend so the final write is next.
            waiter(&g, &[pid, response_write], Some(response_write)).await;
            let _: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                .bind(response_write)
                .fetch_one(&g.pool)
                .await
                .unwrap();
        }
    }
    assert_eq!(g.engine.available_capacity(), full - 1);
    blocker.rollback().await.unwrap();

    let row = tokio::time::timeout(SETTLE_BOUND, g.finished())
        .await
        .expect("session finalization completed");
    assert_eq!(row["state"], "cancelled", "{failure:?}");
    assert_eq!(row["reservation"], "unknown");
    assert_eq!(row["actual"], Value::Null);
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["state"], "unknown");
    assert_eq!(responses[0]["status"], Value::Null, "never settled");
    let window = responses[0]["window"].as_i64().unwrap();
    assert_eq!(row["held"], window);
    let ledger = g.ledger(&id.to_string()).await;
    assert_eq!(settlements(&ledger), vec![&("unknown".into(), Some(0))]);
    assert_eq!(totals(&g).await, vec![(0, window, 0, 1); 2]);
    permits_return(&g, full).await;
}

#[dbtest::test]
async fn drop_then_the_response_write_fails(pool: PgPool) {
    failed_response_write(pool, Failure::Error).await;
}

#[dbtest::test]
async fn drop_then_the_response_write_times_out(pool: PgPool) {
    failed_response_write(pool, Failure::Deadline).await;
}

/// The advisory lock the commit gate waits on; no gateway code takes it.
const COMMIT_GATE: i64 = 0x0742_0803_0000_0001;

/// The owner is aborted while the response insert of its `open_response` is
/// committing (a deferred trigger holds the COMMIT on a lock the test owns).
/// The row commits, but the session never learns it and finishes with the
/// request unopened: the response's window is retained once, not twice.
#[dbtest::test]
async fn an_open_committed_after_its_owner_is_aborted_is_held_once(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let full = g.engine.available_capacity();
    let (mut session, id) = open(&g).await;
    session.reserve_window(100).await.unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION realtime_open_commit_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({COMMIT_GATE}); RETURN NULL; END $$; CREATE CONSTRAINT TRIGGER realtime_open_commit_gate AFTER INSERT ON realtime_responses DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION realtime_open_commit_gate()"))
        .execute(&g.pool)
        .await
        .unwrap();
    let mut gate = g.pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(COMMIT_GATE)
        .execute(&mut *gate)
        .await
        .unwrap();
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();

    let owner = tokio::spawn(async move {
        let _ = session.open_response("one".into()).await;
    });
    // Only the open's COMMIT (its deferred trigger) waits on the gate.
    let open_pid = waiter(&g, &[gate_pid], None).await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    // The dropped session's final write queues behind the committing open,
    // which still holds the reservation row.
    waiter(&g, &[open_pid], None).await;
    let _: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(COMMIT_GATE)
        .fetch_one(&mut *gate)
        .await
        .unwrap();

    let row = tokio::time::timeout(SETTLE_BOUND, g.finished())
        .await
        .expect("session finalization completed");
    assert_eq!(row["state"], "cancelled");
    assert_eq!(row["reservation"], "unknown");
    assert_eq!(row["actual"], Value::Null);
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses.len(), 1, "{responses:?}");
    assert_eq!(responses[0]["sequence"], 1);
    assert_eq!(responses[0]["state"], "unknown");
    let window = responses[0]["window"].as_i64().unwrap();
    assert!(window > 0);
    assert_eq!(row["held"], window);
    let ledger = g.ledger(&id.to_string()).await;
    assert_eq!(settlements(&ledger), vec![&("unknown".into(), Some(0))]);
    assert_eq!(totals(&g).await, vec![(0, window, 0, 1); 2]);
    permits_return(&g, full).await;
}

/// The window held for the session's reservation.
async fn held(g: &Gateway, id: Uuid) -> Option<i64> {
    sqlx::query_scalar("SELECT held_microusd FROM governance_reservations WHERE execution_id=$1")
        .bind(id)
        .fetch_one(&g.pool)
        .await
        .unwrap()
}

/// One request or response at a time: a second window is refused while a
/// forwarded request is unresolved or a response is open, and the armed
/// window is left as it was.
#[dbtest::test]
async fn reserve_window_refuses_while_a_request_or_response_is_in_flight(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let (mut session, id) = open(&g).await;
    session.reserve_window(100).await.unwrap();
    let armed = held(&g, id).await;
    // A smaller window would resize the armed one.
    assert_eq!(
        session.reserve_window(10).await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(held(&g, id).await, armed);
    session.open_response("one".into()).await.unwrap();
    assert_eq!(
        g.responses(&id.to_string()).await[0]["window"],
        json!(armed)
    );
    assert_eq!(
        session.reserve_window(100).await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(held(&g, id).await, armed);
    session
        .settle_response("one", COMPLETED, usage())
        .await
        .unwrap();
    // Once the response is done, and after a rejected request, it is accepted.
    session.reserve_window(100).await.unwrap();
    session.request_rejected();
    session.reserve_window(10).await.unwrap();
    session
        .finish(Outcome::Succeeded, None, None)
        .await
        .unwrap();
}

/// Dropped on a thread without a Tokio runtime, a session spawns nothing:
/// its reservation stays pending for lease reconciliation, which keeps the
/// open response's window held, and its permit goes back with it.
#[dbtest::test]
async fn drop_without_a_runtime_is_left_to_lease_reconciliation(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let full = g.engine.available_capacity();
    let (mut session, id) = open(&g).await;
    session.reserve_window(100).await.unwrap();
    session.open_response("one".into()).await.unwrap();
    std::thread::spawn(move || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(session);
    })
    .join()
    .unwrap();
    assert_eq!(g.engine.available_capacity(), full);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let state: (String, String) = sqlx::query_as("SELECT e.state,r.state FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1")
        .bind(id)
        .fetch_one(&g.pool)
        .await
        .unwrap();
    assert_eq!(state, ("started".into(), "pending".into()));

    let store = Store::new(g.pool.clone());
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(id).execute(&g.pool).await.unwrap();
    assert_eq!(
        crate::governance::reconcile_expired(&store, 10)
            .await
            .unwrap(),
        1
    );
    let row = g.finished().await;
    assert_eq!(row["reservation"], "unknown");
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses[0]["state"], "unknown");
    let window = responses[0]["window"].as_i64().unwrap();
    assert_eq!(row["held"], window);
    assert_eq!(totals(&g).await, vec![(0, window, 0, 1); 2]);
}

/// The line a failed final write logs: it names the execution and, when the
/// write ran under the request's log span, carries that span.
fn finalization_failed(text: &str, id: Uuid) -> Option<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| {
            line["fields"]["message"] == "realtime finalization failed; reconciliation required"
                && line["fields"]["execution_id"] == id.to_string()
        })
}
async fn logged(capture: &crate::log_capture_test::Capture, id: Uuid) -> Value {
    tokio::time::timeout(SETTLE_BOUND, async {
        loop {
            if let Some(line) = finalization_failed(&capture.text(), id) {
                return line;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the failed finalization was logged")
}

/// A dropped session's detached final write keeps the log span it was
/// dropped in.
#[dbtest::test]
async fn a_detached_final_write_keeps_the_request_span(pool: PgPool) {
    use tracing::Instrument;
    let capture = crate::log_capture_test::Capture::start();
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let (session, id) = open(&g).await;
    let (blocker, _) = lock(&g, id).await;
    let span = tracing::info_span!("http.request", request_id = %id);
    let owner = tokio::spawn(
        async move {
            let _session = session;
            std::future::pending::<()>().await;
        }
        .instrument(span),
    );
    tokio::task::yield_now().await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    // The locked row keeps the write waiting past its deadline.
    let line = logged(&capture, id).await;
    blocker.rollback().await.unwrap();
    drop(capture);
    assert_eq!(line["span"]["name"], "http.request", "{line}");
    assert_eq!(line["span"]["request_id"], id.to_string());
}

/// Over the real listener, the session runs under its upgrade request's log
/// span: the failed final write of a client that closed is logged with it.
#[dbtest::test]
async fn a_session_logs_under_its_upgrade_request_span(pool: PgPool) {
    let capture = crate::log_capture_test::Capture::start();
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM inference_executions WHERE workload_kind='realtime' AND state='started'",
    )
    .fetch_one(&g.pool)
    .await
    .unwrap();
    let (blocker, _) = lock(&g, id).await;
    client.close(None).await.unwrap();
    let line = logged(&capture, id).await;
    blocker.rollback().await.unwrap();
    drop(capture);
    assert_eq!(line["span"]["name"], "http.request", "{line}");
    assert_eq!(line["span"]["request_id"], id.to_string());
}

/// A window growth shed behind a held scope row leaves the session's
/// context as it was: the input it would have consumed is the next
/// request's, so the request sent again reserves the window a session that
/// never shed reserves.
#[dbtest::test]
async fn a_shed_window_growth_keeps_its_input_for_the_next_request(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let input = crate::inference::realtime::context::EventSize {
        text_bytes: 4000,
        audio_bytes: 0,
        audio_bytes_per_second: None,
    };
    let held = async |id: Uuid| -> i64 {
        sqlx::query_scalar(
            "SELECT held_microusd FROM governance_reservations WHERE execution_id=$1",
        )
        .bind(id)
        .fetch_one(&g.pool)
        .await
        .unwrap()
    };
    // Shed once, then sent again.
    let (mut shed, shed_id) = open(&g).await;
    shed.client_event(input);
    let row = super::hold_admission_row(&g).await;
    assert_eq!(
        shed.reserve_window(100).await,
        Err(InferenceError::ScopeBusy)
    );
    row.rollback().await.unwrap();
    shed.reserve_window(100).await.unwrap();
    // The same request, never shed.
    let (mut plain, plain_id) = open(&g).await;
    plain.client_event(input);
    plain.reserve_window(100).await.unwrap();
    assert_eq!(held(shed_id).await, held(plain_id).await);
}
