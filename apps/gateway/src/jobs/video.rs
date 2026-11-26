//! Video jobs: create (admission + upstream create), refresh/settle, list,
//! content pass-through and delete. Rendering uses gateway ids only.
use gateway_datetime_compat::UtcInstant;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{store::Observation, *};
use crate::inference::{types::WorkloadKind, workload::OutputReservation};

/// `POST /v1/videos` when no adapter offers video generation.
pub const NO_VIDEO_PROVIDER: &str = "Video generation has no supported provider: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24. An OpenRouter video adapter is planned.";

fn observation(v: &UpstreamVideo) -> Observation<'_> {
    Observation {
        state: v.state,
        upstream_status: v.state.as_str(),
        progress: v.progress,
        completed_at: v.completed_at,
        expires_at: v.expires_at,
        error: v.error.as_ref(),
        counts: None,
    }
}

/// Accounting of a terminal video observation.
pub(crate) fn settlement(job: &JobRow, v: &UpstreamVideo) -> Option<Settlement> {
    let requested = job.video_seconds.and_then(|s| u32::try_from(s).ok())?;
    match v.state {
        JobState::Completed => {
            let variant = v.size.or_else(|| {
                job.video_size
                    .as_deref()
                    .and_then(crate::billing::MeterVariant::new)
            });
            let usage = |ms: Option<u64>| Usage {
                meters: Some(video_meters(ms)),
                output_image_variant: variant,
                ..Usage::default()
            };
            Some(match v.seconds {
                // A longer clip than requested is not covered by the hold:
                // keep the evidence and the hold (never settle it).
                Some(s) if s > requested => Settlement {
                    outcome: Outcome::Failed,
                    error: Some(InferenceError::InvalidUpstream),
                    usage: usage(Some(u64::from(s) * 1000)),
                },
                // Unknown duration stays unknown (hold retained).
                seconds => Settlement {
                    outcome: Outcome::Succeeded,
                    error: None,
                    usage: usage(seconds.map(|s| u64::from(s) * 1000)),
                },
            })
        }
        // The provider accepted and then failed the job: usage stays unknown
        // and the hold is kept even when the price is entirely free.
        JobState::Failed => Some(Settlement {
            outcome: Outcome::Failed,
            error: Some(InferenceError::UpstreamRejected),
            usage: Usage::default(),
        }),
        _ => None,
    }
}

/// How long a video may stay without a provider id before its create counts
/// as interrupted: the create call is bounded at 60 s, with at most one
/// keyed resend, so a create still unanswered after this never returns.
pub(crate) const VIDEO_CREATE_GRACE: TimeDelta = TimeDelta::minutes(10);
/// A record slower than this is not followed by a create: it would leave too
/// little of the grace for the create call and its one resend.
const RECORD_SEND_BOUND: std::time::Duration = std::time::Duration::from_secs(120);

/// A create failure after which the provider may still have the video.
fn unconfirmed(error: InferenceError) -> bool {
    matches!(
        error,
        InferenceError::Timeout
            | InferenceError::UpstreamUnavailable
            | InferenceError::InvalidUpstream
    )
}
/// A create failure a keyed resend may resolve (not an unparseable answer,
/// which a resend would repeat).
fn resendable(error: InferenceError) -> bool {
    matches!(
        error,
        InferenceError::Timeout | InferenceError::UpstreamUnavailable
    )
}
/// The settlement of a video whose create never recorded an outcome: usage
/// unknown, hold kept.
fn interrupted() -> Settlement {
    Settlement {
        outcome: Outcome::Cancelled,
        error: None,
        usage: Usage::default(),
    }
}

pub fn render(job: &JobRow) -> Value {
    let unix = |t: Option<gateway_datetime_compat::UtcInstant>| t.map(|t| t.timestamp());
    let state = job.job_state();
    json!({
        "id": client_id(JobKind::Video.prefix(), job.id),
        "object": "video",
        "model": job.public_model,
        "status": state.as_str(),
        "progress": job.progress.map(i64::from).unwrap_or(if state == JobState::Completed { 100 } else { 0 }),
        "created_at": job.created_at.timestamp(),
        "completed_at": unix(job.completed_at),
        "expires_at": unix(job.expires_at),
        "seconds": job.video_seconds.map(|s| s.to_string()),
        "size": job.video_size,
        "remixed_from_video_id": Value::Null,
        "error": job.error_code.as_ref().filter(|_| state == JobState::Failed).map(|code| json!({"code": code, "message": "Video generation failed"})),
    })
}

impl Jobs {
    pub async fn create_video(
        &self,
        principal: Principal,
        request: VideoRequest,
        request_id: Uuid,
    ) -> JobResult<JobRow> {
        request.validate()?;
        // No registered adapter offers video generation (OpenAI retired its
        // Videos API): refuse explicitly before admission, never a job.
        if !self.registry.supports(ApiProtocol::Videos) {
            return Err(JobError::Conflict(
                "unsupported_capability",
                NO_VIDEO_PROVIDER,
            ));
        }
        let model = request.model.clone();
        let (target, adapter) = self
            .select(
                &principal,
                &model,
                ApiProtocol::Videos,
                request_id,
                |a, d| a.supports_video_request(d, &request),
            )
            .await?;
        let admission = WorkloadAdmission {
            kind: WorkloadKind::Videos,
            output: OutputReservation::None,
            unit_ceilings: unit_ceilings(1, u64::from(request.seconds) * 1000),
        };
        let attempt = self
            .admit(principal, request_id, &model, &target, &admission)
            .await?;
        let size = request.size;
        let seconds = request.seconds;
        let deadline = UtcInstant::now() + VIDEO_POLL_WINDOW;
        let grace = self.video_create_grace();
        let new = store::NewJob {
            id: Uuid::new_v4(),
            kind: JobKind::Video,
            workspace_id: principal.workspace_id,
            api_key_id: principal.key_id,
            deployment_id: target.id,
            execution_id: request_id,
            public_model: &model,
            provider: &target.provider,
            upstream_id: None,
            first_poll_after: grace,
            poll_deadline_at: deadline,
            video_seconds: Some(seconds as i32),
            video_size: Some(size.as_str()),
            batch_endpoint: None,
        };
        let (job, attempt) = self
            .record_video(attempt, &new, request_id, deadline)
            .await?;
        let key = VideoCreateKey::for_job(job.id);
        // Inside the dispatch scope (`inference::dispatch`): a create that
        // built no request proves nothing was sent.
        let (created, nothing_sent) = crate::inference::dispatch::observe(call(
            adapter.create_video_keyed(&target, request.clone(), &key),
        ))
        .await;
        // At most one resend, with the same key, only where the provider makes
        // that the same job and only when the first outcome is unknown. Once
        // an outcome was unknown, the job stays unconfirmed whatever the
        // resend answers.
        let unknown = matches!(created, Err(e) if !nothing_sent && unconfirmed(e));
        let created = match created {
            Err(e)
                if !nothing_sent && resendable(e) && adapter.idempotent_video_create(&target) =>
            {
                call(adapter.create_video_keyed(&target, request, &key))
                    .await
                    .map_err(|resent| (e, resent))
            }
            other => other.map_err(|e| (e, e)),
        };
        let upstream = match created {
            Ok(v) => v,
            // The attempt records the first failure (an unknown outcome stays
            // unknown); the client gets the last one.
            Err((first, last)) => {
                let error = self
                    .create_failed(job, attempt, first, nothing_sent, unknown)
                    .await;
                return Err(match error {
                    JobError::Inference(_) => last.into(),
                    other => other,
                });
            }
        };
        attempt.accepted();
        // The provider has the video: its id is the evidence of acceptance.
        // If this write fails the row stays queued without an id, reported
        // as queued, until the create-grace rule ends it (hold kept).
        let recorded = store::record_video_upstream(&self.store, job.id, &upstream.id)
            .await
            .inspect_err(|_| {
                tracing::error!(execution_id = %request_id, "video provider id not recorded after upstream acceptance; the job ends interrupted with its hold kept");
            })?;
        let Some(job) = recorded else {
            tracing::error!(execution_id = %request_id, "video provider id arrived after the job ended; hold kept");
            return Err(InferenceError::Storage.into());
        };
        // The creator's first observation (`Observer::Creator`): inside the
        // create grace no poller can hold the row (it is first due at the
        // grace's end); after it, a poller that holds the row observes it
        // itself.
        self.apply_video(&job, &upstream, store::Observer::Creator)
            .await
    }

    /// Record first (#662): the job row, without a provider id, and the
    /// extension of its reservation's lease to the poll deadline are one
    /// transaction written before the provider is called, so the gateway
    /// always has a row for a video the provider may have accepted. If this
    /// fails, nothing has been sent: the attempt settles as a known zero. A
    /// record that took long enough for the create-grace rule to end the row
    /// before the provider answers is not sent either.
    async fn record_video(
        &self,
        attempt: Attempt,
        new: &store::NewJob<'_>,
        request_id: Uuid,
        deadline: UtcInstant,
    ) -> JobResult<(JobRow, Attempt)> {
        let recording = std::time::Instant::now();
        let recorded = crate::governance::jobs::accept_job(
            &self.store,
            request_id,
            deadline + LEASE_GRACE,
            async |conn| {
                store::insert_job(conn, new)
                    .await
                    .map_err(|_| InferenceError::Storage)
            },
        )
        .await;
        let job = match recorded {
            Ok(job) => job,
            Err(e) => {
                let (error, recorded) = attempt.fail_unsent(e).await;
                // A COMMIT whose acknowledgement was lost may still have
                // recorded the row: end it now rather than by the grace rule.
                if let Ok(Some(ended)) = store::fail_unrecorded_video(
                    &self.store,
                    new.id,
                    "create_failed",
                    None,
                    store::Observer::Creator,
                )
                .await
                    && recorded
                {
                    let _ = store::mark_settled(&self.store, ended.id).await;
                }
                return Err(error.into());
            }
        };
        if recording.elapsed() > RECORD_SEND_BOUND {
            return Err(self
                .create_failed(job, attempt, InferenceError::Storage, true, false)
                .await);
        }
        Ok((job, attempt))
    }

    /// A create that did not return a job. The attempt is recorded first, so
    /// a concurrent read of the row (still queued, inside the grace) never
    /// settles it differently: nothing sent is a known zero; the provider
    /// answering without a job follows the usual rules; an unknown outcome
    /// keeps the hold. Then the row ends `failed` (`create_failed`, or
    /// `create_unconfirmed` when the provider may have the video) and is
    /// marked settled only if the attempt's record succeeded; otherwise a
    /// later refresh settles it (conservatively, unknown).
    async fn create_failed(
        &self,
        job: JobRow,
        attempt: Attempt,
        error: InferenceError,
        nothing_sent: bool,
        unknown: bool,
    ) -> JobError {
        let code = if unknown {
            "create_unconfirmed"
        } else {
            "create_failed"
        };
        let (error, recorded) = if nothing_sent {
            attempt.fail_unsent(error).await
        } else {
            attempt.fail(error).await
        };
        if let Ok(Some(ended)) =
            store::fail_unrecorded_video(&self.store, job.id, code, None, store::Observer::Creator)
                .await
            && recorded
        {
            let _ = store::mark_settled(&self.store, ended.id).await;
        }
        error.into()
    }

    /// The create grace: how long a video may stay without a provider id
    /// before its create counts as interrupted.
    #[cfg(not(test))]
    pub(crate) fn video_create_grace(&self) -> TimeDelta {
        VIDEO_CREATE_GRACE
    }
    /// Tests may shorten the grace.
    #[cfg(test)]
    pub(crate) fn video_create_grace(&self) -> TimeDelta {
        self.video_create_grace
    }

    /// A video recorded before its create that has no provider id. It is
    /// never sent upstream (there is nothing to poll): while its create may
    /// still be running it stays queued; once the grace has passed without
    /// an outcome it ends `failed` (`create_interrupted`) and settles as
    /// cancelled with unknown usage (hold kept). A terminal one that is not
    /// settled yet (its create failed and the settled mark was lost) is
    /// marked settled; its attempt was already recorded. Each write is made
    /// only for `observer` (a poller under its current claim, or a client
    /// read while no poller holds the row): a stale claim, or a read during
    /// a live claim, changes and settles nothing.
    async fn refresh_unrecorded_video(
        &self,
        job: JobRow,
        observer: store::Observer<'_>,
    ) -> JobResult<JobRow> {
        if job.job_state().is_terminal() {
            if job.settled_at.is_none() && self.settle_as(&job, interrupted(), observer).await? {
                return Ok(store::reload(&self.store, job.id).await?);
            }
            return Ok(job);
        }
        let grace = self.video_create_grace();
        let started = job.submit_started_at.ok_or(InferenceError::Storage)?;
        if UtcInstant::now() - started < grace {
            return Ok(job);
        }
        match store::fail_unrecorded_video(
            &self.store,
            job.id,
            "create_interrupted",
            Some(grace),
            observer,
        )
        .await?
        {
            // Settled under the writer's hold: if the claim ended in
            // between, the ended row is settled by the writer that holds it.
            Some(ended) => {
                self.settle_as(&ended, interrupted(), observer).await?;
                Ok(store::reload(&self.store, ended.id).await?)
            }
            // Its id was recorded meanwhile, it already ended, or another
            // writer holds it.
            None => Ok(store::reload(&self.store, job.id).await?),
        }
    }

    /// Apply an observation and settle a terminal one (idempotent).
    async fn apply_video(
        &self,
        job: &JobRow,
        upstream: &UpstreamVideo,
        observer: store::Observer<'_>,
    ) -> JobResult<JobRow> {
        let interval = self.poll_seconds();
        let (job, recorded) =
            store::observe(&self.store, job, &observation(upstream), interval, observer).await?;
        // Only the observation that was recorded settles, with its own
        // evidence: a refused one leaves settlement to the writer that holds
        // the job.
        if recorded
            && job.job_state().is_terminal()
            && job.settled_at.is_none()
            && let Some(s) = settlement(&job, upstream)
        {
            self.settle_as(&job, s, observer).await?;
            return Ok(store::reload(&self.store, job.id).await?);
        }
        Ok(job)
    }

    /// Settle `job` while `observer` holds its row (`store::hold_writer`), so
    /// no claim can take the job between the writer check and the
    /// settlement. `false` when the observer may not write the row now (the
    /// writer that holds it settles it).
    ///
    /// The hold is the one place in `jobs` that keeps a pool connection while
    /// the settlement takes others from the same pool (the reservation read,
    /// the governance finish, the settled mark). Holders are therefore
    /// bounded per replica (`Store::video_settle_slots`, one at a time), with
    /// the permit taken before the hold's transaction begins: a waiting
    /// settlement holds no connection, so holders can never own every
    /// connection while each waits for another. The lock order is unchanged:
    /// the job row first, then the reservation and totals.
    async fn settle_as(
        &self,
        job: &JobRow,
        finish: Settlement,
        observer: store::Observer<'_>,
    ) -> JobResult<bool> {
        let _slot = self
            .store
            .video_settle_slots
            .acquire()
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut tx = crate::db::begin(&self.store.pool)
            .await
            .map_err(|_| InferenceError::Storage)?;
        if !store::hold_writer(&mut tx, job.id, observer).await? {
            return Ok(false);
        }
        self.settle(job, finish).await?;
        tx.commit().await.map_err(|_| InferenceError::Storage)?;
        Ok(true)
    }

    pub(crate) fn poll_seconds(&self) -> f64 {
        self.limits.poll_interval.map_or(30.0, |d| d.as_secs_f64())
    }

    async fn own_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let id = parse_client_id(JobKind::Video.prefix(), id).ok_or(JobError::NotFound)?;
        store::job(
            &self.store,
            principal.workspace_id,
            JobKind::Video,
            id,
            crate::visibility::Viewer::of(principal),
        )
        .await?
        .ok_or(JobError::NotFound)
    }

    /// Current state, refreshed from the provider while not terminal.
    pub async fn get_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let job = self.own_video(principal, id).await?;
        if job.job_state().is_terminal() && job.settled_at.is_some() {
            return Ok(job);
        }
        self.refresh_video(job, store::Observer::Client).await
    }

    pub(crate) async fn refresh_video(
        &self,
        job: JobRow,
        observer: store::Observer<'_>,
    ) -> JobResult<JobRow> {
        if job.upstream_id.is_none() {
            return self.refresh_unrecorded_video(job, observer).await;
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let upstream = call(adapter.retrieve_video(&target, &upstream_id)).await?;
        self.apply_video(&job, &upstream, observer).await
    }

    pub async fn list_videos(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: i64,
        ascending: bool,
    ) -> JobResult<Vec<JobRow>> {
        let after = after
            .map(|a| parse_client_id(JobKind::Video.prefix(), a).ok_or(JobError::NotFound))
            .transpose()?;
        Ok(store::list(
            &self.store,
            principal.workspace_id,
            JobKind::Video,
            after,
            limit,
            ascending,
            crate::visibility::Viewer::of(principal),
        )
        .await?)
    }

    pub async fn video_content(
        &self,
        principal: &Principal,
        id: &str,
        asset: VideoAsset,
    ) -> JobResult<ContentStream> {
        let job = self.own_video(principal, id).await?;
        if job.job_state() != JobState::Completed {
            return Err(JobError::Conflict(
                "video_not_ready",
                "Video content is available once the video is completed",
            ));
        }
        if job.deleted_at.is_some() {
            return Err(JobError::NotFound);
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        Ok(call(adapter.video_content(&target, &upstream_id, asset)).await?)
    }

    /// Delete a finished video's provider assets. In-progress jobs cannot be
    /// deleted (their accounting is still open).
    pub async fn delete_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let job = self.own_video(principal, id).await?;
        if job.deleted_at.is_some() {
            return Err(JobError::NotFound);
        }
        if !crate::visibility::may_modify_job(
            &self.store.pool,
            principal.workspace_id,
            crate::visibility::Viewer::of(principal),
            job.id,
        )
        .await?
        {
            return Err(JobError::Forbidden(
                "video_not_owned",
                "Only the creator or a workspace admin can delete this video",
            ));
        }
        if !job.job_state().is_terminal() {
            return Err(JobError::Conflict(
                "video_in_progress",
                "A video can be deleted once it is completed or failed",
            ));
        }
        // A video that never got a provider id has no provider assets.
        if job.upstream_id.is_some() {
            let (target, adapter) = self.target(&job).await?;
            let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
            call(adapter.delete_video(&target, &upstream_id)).await?;
        }
        store::mark_deleted(&self.store, job.id).await?;
        Ok(job)
    }
}
