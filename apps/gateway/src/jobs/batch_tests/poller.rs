//! Two pollers on one native batch: the poller's claim is an owner, an epoch
//! and a renewable lease on the job row, and every write the poller makes to a
//! native batch checks that claim in its own transaction. A second poller
//! never refreshes a batch another holds; a poller whose claim ended writes
//! nothing and removes the result files it created.
use super::*;

/// A native batch of two lines, submitted, with a published batch list.
async fn submitted(pool: PgPool) -> (World, JobRow) {
    let w = world(pool, true, false, 2).await;
    v3_price(&w.f, w.f.deployment, true).await;
    let file = w
        .upload(
            &w.f.principal,
            &[
                chat_line("first", "company/smart", "one"),
                chat_line("second", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    w.poll().await;
    let job = w.reload(&job).await;
    assert!(job.upstream_id.is_some());
    (w, job)
}

/// The batch is due again, as it is once the claim's poll interval passed.
async fn due(w: &World, job: &JobRow) {
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// The poller that holds the batch stops renewing: its lease lapses.
async fn lapse(w: &World, job: &JobRow) {
    sqlx::query(
        "UPDATE async_jobs SET runner_lease_until=now()-interval '1 second',next_poll_at=now() WHERE id=$1",
    )
    .bind(job.id)
    .execute(&w.f.store.pool)
    .await
    .unwrap();
}

/// Committed, undeleted result files of the workspace.
async fn live_results(w: &World) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM stored_files WHERE workspace_id=$1 AND api_purpose='batch_output' AND committed_at IS NOT NULL AND deleted_at IS NULL ORDER BY id")
        .bind(w.f.principal.workspace_id)
        .fetch_all(&w.f.store.pool)
        .await
        .unwrap()
}

fn completed(w: &World) {
    let mut s = w.openai.script.lock().unwrap();
    s.batches.push_back(upstream(BatchStatus::Completed));
    s.batches.push_back(upstream(BatchStatus::Completed));
    s.results = vec![
        json!({"custom_id":"l0","ok":true,"input":30,"output":6}),
        json!({"custom_id":"l1","ok":false}),
    ];
}

fn gate(slot: &mut Option<Arc<Gate>>) -> Arc<Gate> {
    slot.insert(Arc::default()).clone()
}

fn count(calls: &[&str], call: &str) -> usize {
    calls.iter().filter(|c| **c == call).count()
}

#[dbtest::test]
async fn a_second_poller_never_refreshes_a_native_batch_another_holds(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    // Poller A reaches collection and reads the results for a long time.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // Its batch is due again while A still works on it: poller B leaves it.
    due(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 0);
    assert_eq!(count(&w.openai.calls(), "retrieve"), 1);
    held.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap(), 1);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    let mut linked: Vec<Uuid> = [job.output_file_id, job.error_file_id]
        .into_iter()
        .flatten()
        .collect();
    linked.sort_unstable();
    assert_eq!(live_results(&w).await, linked);
    assert_eq!(count(&w.openai.calls(), "delete"), 1);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_poller_that_lost_its_claim_writes_nothing_and_removes_its_result_files(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    completed(&w);
    let held = gate(&mut w.openai.script.lock().unwrap().results_gate);
    due(&w, &job).await;
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    // A stalls past its lease: poller B takes the batch over and collects it.
    lapse(&w, &job).await;
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    let collected = w.reload(&job).await;
    assert_eq!(collected.state, "completed");
    assert!(collected.settled_at.is_some());
    let settled = reservation(&w.f, job.execution_id).await;
    // A resumes: it records nothing and deletes the files it wrote.
    held.release.notify_one();
    a.await.unwrap().unwrap();
    let job = w.reload(&job).await;
    assert_eq!(
        (job.output_file_id, job.error_file_id),
        (collected.output_file_id, collected.error_file_id)
    );
    let mut linked: Vec<Uuid> = [job.output_file_id, job.error_file_id]
        .into_iter()
        .flatten()
        .collect();
    linked.sort_unstable();
    assert_eq!(linked.len(), 2);
    assert_eq!(live_results(&w).await, linked);
    assert_eq!(reservation(&w.f, job.execution_id).await, settled);
    // Only the poller that holds the batch deletes the provider's copies.
    assert_eq!(count(&w.openai.calls(), "delete"), 1);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_poller_that_lost_its_claim_never_overwrites_newer_progress(pool: PgPool) {
    let (w, job) = submitted(pool).await;
    let progress = |completed| {
        let mut b = upstream(BatchStatus::InProgress);
        b.counts = Some(RequestCounts {
            total: 2,
            completed,
            failed: 0,
        });
        b
    };
    let held = {
        let mut s = w.openai.script.lock().unwrap();
        s.batches.push_back(progress(1));
        s.batches.push_back(progress(2));
        gate(&mut s.retrieve_gate)
    };
    due(&w, &job).await;
    // A reads one finished line, then stalls before it records it.
    let a = {
        let jobs = w.jobs.clone();
        tokio::spawn(async move { jobs.poll_once().await })
    };
    held.entered.notified().await;
    lapse(&w, &job).await;
    // B takes the batch over and records two.
    assert_eq!(w.jobs.poll_once().await.unwrap(), 1);
    assert_eq!(w.reload(&job).await.request_completed, Some(2));
    held.release.notify_one();
    a.await.unwrap().unwrap();
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.upstream_status.as_deref()),
        (Some(2), Some("in_progress"))
    );
}
