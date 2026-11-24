//! Native batches (0021): one upstream attempt on a provider batch API,
//! driven by the poller.
//!
//! 1. **Submit** (once): the batch's private copy is streamed line by line,
//!    each line encoded by the adapter (`l<n>` ids, upstream model) and sent
//!    as one batch. The claim (`submit_started_at`) is exclusive with an
//!    early cancel; an interrupted submission is never repeated (it may have
//!    reached the provider): it fails with the hold retained.
//! 2. **Poll**: state, counts and a pending cancel are applied.
//! 3. **Collect** (terminal upstream state): results are decoded into the
//!    client's endpoint shape and written to gateway `batch_output` files,
//!    lines without a result go to the error file, usage is summed per line
//!    (the provider's aggregate when it reports one) and the reservation
//!    settles at its pinned price list. Then the provider's copies and the
//!    private copy are deleted.
//!
//! Every write to the batch's row up to and including the finish runs under
//! the poller's claim of the batch ([`BatchFence`]), checked inside the
//! transaction that makes it. A poller whose claim ended before its finish
//! committed writes nothing more to the row: it does not submit, record a
//! status or counts, or finish the batch, and it deletes the result files it
//! wrote. Once a lease renewal is refused, a collection also stops at its
//! next safe point (before each result line, before the lines without a
//! result, after the files are committed) and deletes what it wrote.
//!
//! Two things are not fenced. The upstream id of an accepted submission is
//! recorded whatever the claim, because it is the evidence of acceptance.
//! And everything after a committed finish: settlement (idempotent), the
//! deletion of the provider's copies and of the private copy. A poller whose
//! lease lapses between its finish and its settlement therefore still
//! settles the batch and deletes the provider's copies, possibly while
//! another poller collects it again; that collection finds the batch
//! terminal, keeps the first collection's links and deletes its own files.
use super::{
    batch::settlement,
    fence::{BatchFence, FenceError, FenceResult},
    io::{LazySink, LineReader, RESULT_LINE_BYTES, Source},
    lines::{self, NotRun},
    store::Finished,
    *,
};
use gateway_datetime_compat::UtcInstant;
use std::sync::atomic::{AtomicBool, Ordering};

/// Longer than one submission may take; afterwards an unfinished submission
/// is treated as interrupted.
pub(crate) const SUBMIT_GRACE: TimeDelta = TimeDelta::minutes(40);

/// `None` when the claim ended: another poller holds the batch.
fn held<T>(fence: &BatchFence, written: FenceResult<T>) -> JobResult<Option<T>> {
    match written {
        Ok(value) => Ok(Some(value)),
        Err(FenceError::Stale) => {
            stopped(fence);
            Ok(None)
        }
        Err(_) => Err(InferenceError::Storage.into()),
    }
}

/// The poller stops working on a batch whose claim ended.
fn stopped(fence: &BatchFence) {
    tracing::warn!(job_id = %fence.job, "batch poller no longer holds its batch; it stops working on it");
}

/// Whether the poller's lease renewal found the claim ended.
fn lost_claim(lost: &AtomicBool) -> bool {
    lost.load(Ordering::SeqCst)
}

impl Jobs {
    #[cfg(not(test))]
    fn submit_grace(&self) -> TimeDelta {
        SUBMIT_GRACE
    }
    /// Tests may shorten the grace.
    #[cfg(test)]
    fn submit_grace(&self) -> TimeDelta {
        self.submit_grace
    }

    /// One poller step for a native engine batch.
    /// `lost` is set once a lease renewal found the claim ended.
    pub(crate) async fn poll_native(
        &self,
        job: JobRow,
        fence: BatchFence,
        lost: &AtomicBool,
    ) -> JobResult<()> {
        if job.upstream_id.is_none() {
            return match job.submit_started_at {
                None if job.cancel_requested_at.is_none() => self.submit_native(job, fence).await,
                None => Ok(()),
                Some(at) if UtcInstant::now() - at > self.submit_grace() => {
                    self.submission_lost(fence).await
                }
                Some(_) => Ok(()),
            };
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let mut upstream = call(adapter.retrieve_batch(&target, &upstream_id)).await?;
        if job.cancel_requested_at.is_some()
            && !upstream.status.state().is_terminal()
            && upstream.status != BatchStatus::Cancelling
        {
            upstream = call(adapter.cancel_batch(&target, &upstream_id)).await?;
        }
        let state = upstream.status.state();
        if !state.is_terminal() {
            let observed = fence
                .observe_native(
                    &self.store,
                    upstream.counts,
                    state == JobState::InProgress,
                    upstream.status.as_str(),
                    self.poll_seconds(),
                )
                .await;
            held(&fence, observed)?;
            return Ok(());
        }
        self.collect_native(job, fence, lost, &target, adapter.as_ref(), &upstream)
            .await
    }

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    async fn submit_native(&self, job: JobRow, fence: BatchFence) -> JobResult<()> {
        if held(&fence, fence.claim_submit(&self.store).await)? != Some(true) {
            return Ok(());
        }
        let files = self.files()?.clone();
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let (target, adapter) = self.target(&job).await?;
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, InferenceError>>(8);
        let records: ByteStream = Box::pin(async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        });
        let encoder = {
            let adapter = adapter.clone();
            let target = target.clone();
            let workspace = job.workspace_id;
            let store = self.store.clone();
            async move {
                let produced = async {
                    let mut reader = LineReader::stored(&files, work, workspace, WORK_LINE_BYTES)
                        .await
                        .map_err(|_| InferenceError::Storage)?;
                    let mut n = 0u32;
                    let mut bounds = std::collections::HashMap::new();
                    while let Some(raw) = reader.next_request().await? {
                        let mut line = lines::parse_work_line(&raw, endpoint)
                            .map_err(|_| InferenceError::InvalidRequest)?;
                        if let Some(plan) = line.plan {
                            if plan.deployment != target.id {
                                return Err(InferenceError::Configuration);
                            }
                            if let BatchRequest::Chat(r) = &mut line.request {
                                let cap = match bounds.get(&plan.price) {
                                    Some(cap) => *cap,
                                    None => {
                                        let cap = crate::governance::batch::planned_output(
                                            &store,
                                            plan.price,
                                            target.id,
                                            adapter.generation_output_limit(),
                                        )
                                        .await?;
                                        bounds.insert(plan.price, cap);
                                        cap
                                    }
                                };
                                r.max_output_tokens = Some(cap);
                            }
                        }
                        let record = adapter.encode_native_line(
                            &target,
                            endpoint,
                            &format!("l{n}"),
                            &line.request,
                        )?;
                        tx.send(Ok(record.into()))
                            .await
                            .map_err(|_| InferenceError::UpstreamUnavailable)?;
                        n += 1;
                    }
                    Ok::<_, InferenceError>(n)
                }
                .await;
                if let Err(e) = &produced {
                    // Abort the upstream body: the provider never gets a full batch.
                    let _ = tx.send(Err(*e)).await;
                }
                produced
            }
        };
        let submit = tokio::time::timeout(
            UPLOAD_TIMEOUT,
            adapter.submit_native_batch(&target, endpoint, records),
        );
        let (submitted, produced) = tokio::join!(submit, encoder);
        let outcome = match (submitted, produced) {
            (Ok(Ok(upstream)), produced) => {
                // An accepted response proves acceptance even if the concurrent
                // encoder failed. Persist its id before classifying that failure.
                if !store::set_upstream(&self.store, job.id, &upstream.id).await? {
                    // The batch ended while the provider was accepting it
                    // (its hold is kept as unknown cost): the provider batch
                    // runs and is billed but is never polled. Counted and
                    // logged with the provider's id so an operator can cancel it.
                    crate::metrics::METRICS.observe_batch_late_submission();
                    tracing::warn!(
                        job_id = %job.id,
                        upstream_id = %upstream.id.as_str(),
                        "native batch accepted after it ended; the provider batch is not polled"
                    );
                    return Ok(());
                }
                produced.map(|_| upstream)
            }
            (Ok(Err(e)), _) => Err(e),
            (Err(_), _) => Err(InferenceError::Timeout),
        };
        match outcome {
            Ok(upstream) => {
                crate::metrics::METRICS.observe_batch_submitted(&job.provider);
                if !upstream.status.state().is_terminal() {
                    let status = fence.native_status(&self.store, None, upstream.status.as_str());
                    held(&fence, status.await)?;
                }
                Ok(())
            }
            Err(e) => {
                tracing::warn!(job_id = %job.id, "native batch submission failed");
                // A claim that ended leaves the started submission to the
                // claim that holds the batch: it ends interrupted after the
                // grace, with its hold retained.
                let finished = fence
                    .finish_native(
                        &self.store,
                        &Finished {
                            state: JobState::Failed,
                            error_code: Some("submission_failed"),
                            output_file: None,
                            error_file: None,
                            counts: None,
                        },
                    )
                    .await;
                let Some((job, recorded)) = held(&fence, finished)? else {
                    return Ok(());
                };
                // Partial submission may have reached the provider. Only an explicit
                // pre-processing rejection can qualify for free-zero settlement.
                self.settle(
                    &job,
                    Settlement {
                        outcome: Outcome::Failed,
                        error: Some(e),
                        usage: Usage::default(),
                    },
                )
                .await?;
                self.discard_work_file(&job).await;
                if recorded {
                    crate::metrics::METRICS
                        .observe_batch_finished("native", JobState::Failed.as_str());
                }
                Ok(())
            }
        }
    }

    /// A submission that never recorded its upstream id (crash): never
    /// retried; the batch fails and its hold is retained as unknown.
    async fn submission_lost(&self, fence: BatchFence) -> JobResult<()> {
        let finished = fence
            .finish_native(
                &self.store,
                &Finished {
                    state: JobState::Failed,
                    error_code: Some("submission_interrupted"),
                    output_file: None,
                    error_file: None,
                    counts: None,
                },
            )
            .await;
        let Some((job, _)) = held(&fence, finished)? else {
            return Ok(());
        };
        self.settle(
            &job,
            Settlement {
                outcome: Outcome::Cancelled,
                error: None,
                usage: Usage::default(),
            },
        )
        .await?;
        self.discard_work_file(&job).await;
        Ok(())
    }

    /// The client `custom_id`s of the batch, in line order.
    async fn custom_ids(&self, job: &JobRow) -> JobResult<Vec<String>> {
        let files = self.files()?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let mut reader = LineReader::stored(files, work, job.workspace_id, WORK_LINE_BYTES)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut ids = Vec::new();
        while let Some(raw) = reader.next_request().await? {
            ids.push(lines::custom_id(&raw).ok_or(InferenceError::Storage)?);
        }
        Ok(ids)
    }

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    async fn collect_native(
        &self,
        job: JobRow,
        fence: BatchFence,
        lost: &AtomicBool,
        target: &Deployment,
        adapter: &dyn ProviderAdapter,
        upstream: &UpstreamBatch,
    ) -> JobResult<()> {
        // A batch whose finish committed and whose settlement did not (its
        // poller stopped in between): with the provider's aggregate usage it
        // settles from that, without reading the results again. Without it
        // the per-line sum is the only usage evidence, so the results are
        // read again below, and the files this second collection writes are
        // removed because the finish keeps the first collection's links.
        let recorded = job.job_state();
        if recorded.is_terminal()
            && let Some(usage) = upstream.usage
        {
            if let Some(s) = settlement(recorded, Some(usage)) {
                self.settle(&job, s).await?;
            }
            if call(adapter.delete_native_batch(target, upstream))
                .await
                .is_err()
            {
                tracing::warn!(job_id = %job.id, "provider batch copies not deleted");
            }
            self.discard_work_file(&job).await;
            return Ok(());
        }
        let files = self.files()?.clone();
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        let finalizing = fence.native_status(&self.store, upstream.counts, "finalizing");
        if held(&fence, finalizing.await)?.is_none() {
            return Ok(());
        }
        let ids = self.custom_ids(&job).await?;
        let hex = job.id.simple();
        let mut out = LazySink::new(
            &files,
            io::batch_file(
                false,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{hex}_output.jsonl"),
            ),
        );
        let mut err = LazySink::new(
            &files,
            io::batch_file(
                false,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{hex}_error.jsonl"),
            ),
        );
        let state = upstream.status.state();
        // `Ok(None)`: the claim was lost; the sinks are aborted below.
        let written = async {
            let results = call(adapter.native_batch_results(target, upstream)).await?;
            let mut reader = LineReader::new(Source::Upstream(results), RESULT_LINE_BYTES);
            let mut seen = vec![false; ids.len()];
            let mut usage: Option<Usage> = None;
            let mut usage_known = true;
            let mut succeeded = 0u32;
            // Safe points: before each result line and before the lines
            // without a result.
            while !lost_claim(lost)
                && let Some(raw) = reader.next_request().await?
            {
                let Ok(result) = adapter.decode_native_result(target, endpoint, &raw) else {
                    usage_known = false;
                    continue;
                };
                let index = result
                    .custom_id
                    .strip_prefix('l')
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n < ids.len() && !seen[*n]);
                let Some(index) = index else {
                    usage_known = false;
                    continue;
                };
                seen[index] = true;
                let custom_id = &ids[index];
                match result.outcome {
                    NativeOutcome::Succeeded(response) => {
                        succeeded += 1;
                        let u = response.usage();
                        if u.input_tokens.is_none() || u.output_tokens.is_none() {
                            usage_known = false;
                        }
                        usage = Some(usage.map_or(u, |a| add_usage(a, u)));
                        let rid = Uuid::new_v4();
                        match lines::response_body(endpoint, rid, &job.public_model, *response) {
                            Ok(body) => {
                                out.line(&lines::success_line(custom_id, None, body))
                                    .await?
                            }
                            Err(e) => {
                                err.line(&lines::failure_line(custom_id, None, endpoint, e))
                                    .await?
                            }
                        }
                    }
                    NativeOutcome::Failed { status, code } => {
                        err.line(&lines::provider_failure_line(
                            custom_id, endpoint, status, &code,
                        ))
                        .await?
                    }
                    NativeOutcome::Cancelled => {
                        err.line(&lines::not_run_line(custom_id, NotRun::Cancelled))
                            .await?
                    }
                    NativeOutcome::Expired => {
                        err.line(&lines::not_run_line(custom_id, NotRun::Expired))
                            .await?
                    }
                }
            }
            if lost_claim(lost) {
                return Ok(None);
            }
            let missing = match state {
                JobState::Cancelled => NotRun::Cancelled,
                JobState::Expired => NotRun::Expired,
                JobState::Failed => NotRun::Failed,
                _ => NotRun::ResultUnavailable,
            };
            for (index, custom_id) in ids.iter().enumerate() {
                if !seen[index] {
                    err.line(&lines::not_run_line(custom_id, missing)).await?;
                }
            }
            Ok::<_, InferenceError>(Some((usage.filter(|_| usage_known), succeeded)))
        }
        .await;
        let (summed, succeeded) = match written {
            Ok(Some(v)) if !lost_claim(lost) => v,
            Ok(_) => {
                out.abort().await;
                err.abort().await;
                stopped(&fence);
                return Ok(());
            }
            Err(e) => {
                out.abort().await;
                err.abort().await;
                return Err(e.into());
            }
        };
        // A failed output commit must not leave the error sink to commit a
        // file by being dropped.
        let output_file = match out.finish().await {
            Ok(file) => file,
            Err(e) => {
                err.abort().await;
                return Err(e.into());
            }
        };
        // A committed output file that no batch will link is removed with
        // the error file's failure, as the runner does.
        let error_file = match err.finish().await {
            Ok(file) => file,
            Err(e) => {
                super::runner::remove_files(&files, &job, [output_file, None]).await;
                return Err(e.into());
            }
        };
        if lost_claim(lost) {
            super::runner::remove_files(&files, &job, [output_file, error_file]).await;
            stopped(&fence);
            return Ok(());
        }
        let total = ids.len() as u32;
        let counts = RequestCounts {
            total,
            completed: succeeded,
            failed: upstream
                .counts
                .map_or(total - succeeded.min(total), |c| c.failed),
        };
        let finished = Finished {
            state,
            error_code: (state == JobState::Failed).then_some("batch_failed"),
            output_file,
            error_file,
            counts: Some(counts),
        };
        let Some((job, recorded)) = self.publish_native(&files, job, fence, &finished).await?
        else {
            return Ok(());
        };
        // Counted by the claim that recorded the finish only: a lost claim
        // records and counts nothing, and a collection of a batch that is
        // finished already (its first poller's settlement did not commit)
        // counts nothing again.
        if recorded {
            #[cfg(test)]
            self.finish_metrics
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::metrics::METRICS.observe_batch_lines(
                "native",
                &job.provider,
                u64::from(succeeded),
                u64::from(counts.failed),
            );
        }
        #[cfg(test)]
        self.settle_hold.pause().await;
        // The provider's aggregate when it reports one, else the per-line sum
        // when every succeeded line reported usage; otherwise unknown.
        let usage = upstream.usage.or(summed);
        if let Some(s) = settlement(state, usage) {
            self.settle(&job, s).await?;
        }
        if call(adapter.delete_native_batch(target, upstream))
            .await
            .is_err()
        {
            tracing::warn!(job_id = %job.id, "provider batch copies not deleted");
        }
        self.discard_work_file(&job).await;
        if recorded {
            crate::metrics::METRICS.observe_batch_finished("native", state.as_str());
        }
        Ok(())
    }

    /// Record the collected results under the claim. `None` when the claim
    /// ended: nothing was recorded and the result files are deleted. Files a
    /// batch that finished earlier does not link are deleted too.
    async fn publish_native(
        &self,
        files: &FileStorage,
        job: JobRow,
        fence: BatchFence,
        f: &Finished<'_>,
    ) -> JobResult<Option<(JobRow, bool)>> {
        let created = [f.output_file, f.error_file];
        match fence.finish_native(&self.store, f).await {
            Ok((row, recorded)) => {
                let unused = created.map(|f| {
                    f.filter(|f| row.output_file_id != Some(*f) && row.error_file_id != Some(*f))
                });
                super::runner::remove_files(files, &row, unused).await;
                Ok(Some((row, recorded)))
            }
            Err(FenceError::Stale) => {
                stopped(&fence);
                super::runner::remove_files(files, &job, created).await;
                Ok(None)
            }
            Err(FenceError::Storage) => {
                super::runner::remove_files(files, &job, created).await;
                Err(InferenceError::Storage.into())
            }
            // The links may have committed, or a file is locked by another
            // operation: the sweeper reclaims what no batch links.
            Err(FenceError::Unknown | FenceError::FileBusy) => Err(InferenceError::Storage.into()),
        }
    }
}
