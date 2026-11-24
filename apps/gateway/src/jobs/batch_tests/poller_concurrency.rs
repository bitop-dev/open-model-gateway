//! The poller refreshes several jobs at once (#802): one native collection
//! that stalls for longer than a tick holds one slot, and the other jobs'
//! status refreshes and cancels go on in the other slots. Every job is still
//! claimed once (owner, lease and epoch), each tick claims its free slots in
//! one statement, and dropping the poller cancels the work it has in flight.
use std::time::{Duration, Instant};

use super::*;

/// Three native batches submitted with distinct upstream ids.
async fn three_batches(pool: PgPool) -> (World, Vec<JobRow>) {
    let w = world(pool, true, false, 2).await;
    v3_price(&w.f, w.f.deployment, true).await;
    w.openai.script.lock().unwrap().unique_upstream_ids = true;
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
        assert!(row.upstream_id.is_some(), "submitted");
        rows.push(row);
    }
    (w, rows)
}

fn answer(job: &JobRow, status: BatchStatus, completed: u32) -> UpstreamBatch {
    let mut batch = upstream(status);
    batch.id = job.upstream().unwrap();
    batch.counts = Some(RequestCounts {
        total: 1,
        completed,
        failed: 0,
    });
    batch
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

/// One collection blocked for many ticks does not delay another job's status
/// refresh or a third job's cancel, and dropping the poller cancels the
/// blocked collection's upstream call.
#[dbtest::test]
async fn one_stalled_collection_does_not_delay_other_jobs(pool: PgPool) {
    let (mut w, rows) = three_batches(pool).await;
    let (stalls, refreshes, cancels) = (&rows[0], &rows[1], &rows[2]);
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        s.by_id.insert(
            stalls.upstream_id.clone().unwrap(),
            answer(stalls, BatchStatus::Completed, 1),
        );
        s.by_id.insert(
            refreshes.upstream_id.clone().unwrap(),
            answer(refreshes, BatchStatus::InProgress, 0),
        );
        s.by_id.insert(
            cancels.upstream_id.clone().unwrap(),
            answer(cancels, BatchStatus::InProgress, 0),
        );
        s.results = vec![json!({"custom_id":"l0","ok":true,"input":3,"output":1})];
        s.results_gate.insert(Arc::default()).clone()
    };
    // The stalling batch is due first, so it is claimed first.
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
    // Submission left it `validating`: `in_progress` is the refresh.
    assert_eq!(
        w.reload(refreshes).await.upstream_status.as_deref(),
        Some("validating")
    );
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    // The first batch's collection is now blocked in its results download.
    tokio::time::timeout(Duration::from_secs(10), held.entered.notified())
        .await
        .expect("the stalled collection started");
    // While it stays blocked for many ticks, the second batch's status is
    // refreshed and the third batch's cancel reaches the provider.
    sqlx::query("UPDATE async_jobs SET cancel_requested_at=now(),next_poll_at=now() WHERE id=$1")
        .bind(cancels.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    until("the other status refresh", tick * 30, async || {
        w.reload(refreshes).await.upstream_status.as_deref() == Some("in_progress")
    })
    .await;
    until("the cancel", tick * 30, async || {
        w.openai.calls().contains(&"cancel")
    })
    .await;
    let blocked = w.reload(stalls).await;
    assert_eq!(
        (blocked.upstream_status.as_deref(), blocked.settled_at),
        (Some("finalizing"), None),
        "the stalled collection is still blocked"
    );
    // Dropping the poller drops the blocked collection: its upstream call is
    // cancelled, nothing is recorded for it, and its hold is kept.
    poller.abort();
    tokio::time::timeout(Duration::from_secs(10), held.dropped.notified())
        .await
        .expect("the blocked upstream call was cancelled");
    let stalled = w.reload(stalls).await;
    assert!(stalled.settled_at.is_none());
    assert!(stalled.output_file_id.is_none());
    consistent(&w.f).await;
}

/// A pass claims all its free slots in one statement: three due jobs and
/// four slots take one claim statement.
#[dbtest::test]
async fn a_pass_claims_its_free_slots_in_one_statement(pool: PgPool) {
    let (w, rows) = three_batches(pool).await;
    {
        let mut s = w.openai.script.lock().unwrap();
        for row in &rows {
            s.by_id.insert(
                row.upstream_id.clone().unwrap(),
                answer(row, BatchStatus::InProgress, 0),
            );
        }
    }
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let before = w
        .jobs
        .claim_statements
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(w.jobs.poll_once().await.unwrap(), 3);
    let after = w
        .jobs
        .claim_statements
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(after - before, 1);
}

/// A claim that comes back empty while jobs are due costs at most one tick:
/// the next tick claims them, as for any job that became due just after a
/// claim.
#[dbtest::test]
async fn an_empty_claim_costs_at_most_one_tick(pool: PgPool) {
    let (mut w, rows) = three_batches(pool).await;
    {
        let mut s = w.openai.script.lock().unwrap();
        for row in &rows {
            s.by_id.insert(
                row.upstream_id.clone().unwrap(),
                answer(row, BatchStatus::InProgress, 0),
            );
        }
    }
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.jobs
        .empty_claims
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    until("all three refreshed", tick * 20, async || {
        let polled: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM async_jobs WHERE id=ANY($1) AND last_polled_at IS NOT NULL AND upstream_status='in_progress'",
        )
        .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        polled == 3
    })
    .await;
    poller.abort();
}

/// The poller's concurrency is a validated setting.
#[test]
fn the_poll_concurrency_is_validated() {
    let limits = |v: Option<&str>| {
        JobLimits::from_lookup(|name| {
            (name == "GATEWAY_JOB_POLL_CONCURRENCY")
                .then(|| v.map(str::to_owned))
                .flatten()
        })
    };
    assert_eq!(limits(None).unwrap().poll_concurrency, 4);
    assert_eq!(limits(Some("1")).unwrap().poll_concurrency, 1);
    assert_eq!(limits(Some("32")).unwrap().poll_concurrency, 32);
    for bad in ["0", "33", "-1", "x"] {
        assert!(limits(Some(bad)).is_err(), "{bad}");
    }
}

/// The stalled batch of [`three_batches`], due first, its collection blocked
/// in the results download; the others answer `in_progress`.
fn stall_first(w: &World, rows: &[JobRow]) -> Arc<Gate> {
    let mut s = w.openai.script.lock().unwrap();
    s.by_id.insert(
        rows[0].upstream_id.clone().unwrap(),
        answer(&rows[0], BatchStatus::Completed, 1),
    );
    for row in &rows[1..] {
        s.by_id.insert(
            row.upstream_id.clone().unwrap(),
            answer(row, BatchStatus::InProgress, 0),
        );
    }
    s.results = vec![json!({"custom_id":"l0","ok":true,"input":3,"output":1})];
    s.results_gate.insert(Arc::default()).clone()
}
async fn first_due(w: &World, first: &JobRow) {
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id<>$1")
        .bind(first.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()-interval '1 minute' WHERE id=$1")
        .bind(first.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}
async fn epoch(w: &World, job: &JobRow) -> i64 {
    sqlx::query_scalar("SELECT runner_epoch FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap()
}

/// A claim statement that waits (a busy pool) does not stop the refreshes
/// in flight: their lease renewals go on, so their claims do not lapse.
#[dbtest::test]
async fn a_claim_in_flight_does_not_stop_the_running_refreshes(pool: PgPool) {
    use crate::jobs::poller::{PollLease, Renewal};
    let (mut w, rows) = three_batches(pool).await;
    w.jobs.poll_lease = PollLease {
        seconds: 2.0,
        renew: Duration::from_millis(200),
    };
    let held = stall_first(&w, &rows);
    first_due(&w, &rows[0]).await;
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), held.entered.notified())
        .await
        .expect("the stalled collection started");
    // The same poller's next claim blocks before its statement, while its
    // stalled refresh is still in flight.
    w.jobs.claim_hold.arm();
    tokio::time::timeout(Duration::from_secs(10), w.jobs.claim_hold.reached())
        .await
        .expect("a claim is blocked");
    let renewed = w.jobs.renewals(Renewal::Renewed);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        w.jobs.renewals(Renewal::Renewed) > renewed,
        "the stalled refresh keeps renewing while the claim waits"
    );
    w.jobs.claim_hold.release();
    poller.abort();
}

/// A poller never claims a job it is still refreshing, even when that
/// refresh's lease has lapsed: the job is not collected twice by one process.
#[dbtest::test]
async fn a_poller_never_claims_a_job_it_is_refreshing(pool: PgPool) {
    let (mut w, rows) = three_batches(pool).await;
    let held = stall_first(&w, &rows);
    first_due(&w, &rows[0]).await;
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), held.entered.notified())
        .await
        .expect("the stalled collection started");
    let claimed = epoch(&w, &rows[0]).await;
    // Its lease lapses (renewals are 15 s apart here) and it is due again.
    sqlx::query("UPDATE async_jobs SET runner_lease_until=now()-interval '1 second',next_poll_at=now() WHERE id=$1")
        .bind(rows[0].id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(epoch(&w, &rows[0]).await, claimed, "not claimed again");
    poller.abort();
}

/// A claim that fails while refreshes run does not abandon them: the pass
/// lets them end their claims, then reports the failure.
#[dbtest::test]
async fn a_failed_claim_lets_the_running_refreshes_end_their_claims(pool: PgPool) {
    let (mut w, rows) = three_batches(pool).await;
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        for _ in 0..3 {
            s.batches.push_back(upstream(BatchStatus::InProgress));
        }
        s.retrieve_gate.insert(Arc::default()).clone()
    };
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.jobs.limits.poll_concurrency = 2;
    // The pass's second claim statement fails.
    let made = w
        .jobs
        .claim_statements
        .load(std::sync::atomic::Ordering::SeqCst);
    w.jobs
        .failing_claim
        .store(made + 2, std::sync::atomic::Ordering::SeqCst);
    let pass = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    tokio::time::timeout(Duration::from_secs(10), held.entered.notified())
        .await
        .expect("one refresh is held");
    until("the second claim", Duration::from_secs(10), async || {
        w.jobs
            .claim_statements
            .load(std::sync::atomic::Ordering::SeqCst)
            >= made + 2
    })
    .await;
    held.release.notify_one();
    assert!(pass.await.unwrap().is_err(), "the failed claim is reported");
    let leased: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM async_jobs WHERE id=ANY($1) AND runner_lease_until IS NOT NULL",
    )
    .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(leased, 0, "every claim was ended");
}

/// A refresh that panics ends only its own work: the poller goes on with
/// the other jobs.
#[dbtest::test]
async fn a_panicking_refresh_does_not_stop_the_poller(pool: PgPool) {
    let (mut w, rows) = three_batches(pool).await;
    {
        let mut s = w.openai.script.lock().unwrap();
        for row in &rows {
            s.by_id.insert(
                row.upstream_id.clone().unwrap(),
                answer(row, BatchStatus::InProgress, 0),
            );
        }
    }
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    w.jobs
        .panicking_refreshes
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let tick = Duration::from_millis(100);
    w.jobs.limits.poll_interval = Some(tick);
    let poller = crate::jobs::poller::start(w.jobs.clone()).unwrap();
    until("the other jobs refreshed", tick * 30, async || {
        let polled: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM async_jobs WHERE id=ANY($1) AND upstream_status='in_progress'",
        )
        .bind(rows.iter().map(|r| r.id).collect::<Vec<_>>())
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        polled >= 2
    })
    .await;
    tokio::time::sleep(tick * 5).await;
    assert!(!poller.is_finished(), "the poller is still running");
    poller.abort();
}
