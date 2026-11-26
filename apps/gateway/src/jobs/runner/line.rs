//! One claimed line's execution: why a batch stops, its shared run state,
//! and running a line (with its explicit retries) through the engine.
use super::*;
use crate::jobs::{fence::UndispatchedRetry, schedule::claims::RetryClaim};
use gateway_datetime_compat::UtcInstant;

/// Why a batch stopped dispatching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    Cancelled,
    Expired,
    Budget,
    /// The batch's envelope reservation is no longer pending. No line
    /// can be admitted without a pending envelope.
    HoldUnavailable,
    /// The run lost its claim or doubts its lease. Never a reason to end the
    /// batch: the run leaves it for the claim that holds it next.
    Lost,
}

pub(super) struct RunState {
    /// This run's claim of the batch; every write it makes carries it.
    pub(super) fence: BatchFence,
    pub(super) cancel: AtomicBool,
    /// The run stops working on the batch: the fence refused a write, or the
    /// lease can no longer be trusted.
    pub(super) lost: AtomicBool,
    /// The fence refused a write: the batch belongs to another claim, so
    /// nothing the run holds may be stored.
    pub(super) fenced: AtomicBool,
    /// Signalled when the lease is in doubt: the segment writer stores what
    /// it holds now, before a takeover can refuse it.
    pub(super) doubt: tokio::sync::Notify,
    pub(super) budget: AtomicBool,
    pub(super) hold_unavailable: AtomicBool,
    pub(super) busy_streak: AtomicU32,
    pub(super) pause_until: Mutex<Option<tokio::time::Instant>>,
    #[cfg(test)]
    pub(super) pauses: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only: turns the next finished line into a soft loss.
    #[cfg(test)]
    pub(super) soft_loss: Arc<AtomicBool>,
    /// Test-only flush age of the run's result segments (`Jobs::segment_age`).
    #[cfg(test)]
    pub(super) segment_age: Option<std::time::Duration>,
}
impl RunState {
    pub(super) fn new(fence: BatchFence) -> Self {
        Self {
            fence,
            cancel: AtomicBool::default(),
            lost: AtomicBool::default(),
            fenced: AtomicBool::default(),
            doubt: tokio::sync::Notify::new(),
            budget: AtomicBool::default(),
            hold_unavailable: AtomicBool::default(),
            busy_streak: AtomicU32::default(),
            pause_until: Mutex::default(),
            #[cfg(test)]
            pauses: Arc::default(),
            #[cfg(test)]
            soft_loss: Arc::default(),
            #[cfg(test)]
            segment_age: None,
        }
    }
    /// The batch belongs to another claim: the run writes nothing more to it.
    pub(super) fn lose(&self) {
        self.lost.store(true, Ordering::SeqCst);
        if !self.fenced.swap(true, Ordering::SeqCst) {
            tracing::warn!(job_id = %self.fence.job, "batch runner no longer holds its batch; it stops working on it");
        }
    }
    /// A fenced write's result: a refused write ends the run's work.
    pub(super) fn written<T>(&self, result: FenceResult<T>) -> JobResult<Option<T>> {
        match result {
            Ok(v) => Ok(Some(v)),
            Err(FenceError::Stale) => {
                self.lose();
                Ok(None)
            }
            Err(FenceError::Storage | FenceError::Unknown | FenceError::FileBusy) => {
                Err(InferenceError::Storage.into())
            }
        }
    }
    pub(super) fn stop_reason(&self, job: &JobRow) -> Option<Stop> {
        if self.lost.load(Ordering::SeqCst) {
            Some(Stop::Lost)
        } else if self.budget.load(Ordering::SeqCst) {
            Some(Stop::Budget)
        } else if self.cancel.load(Ordering::SeqCst) {
            Some(Stop::Cancelled)
        } else if UtcInstant::now() >= job.created_at + job.completion_window() {
            Some(Stop::Expired)
        } else if self.hold_unavailable.load(Ordering::SeqCst) {
            Some(Stop::HoldUnavailable)
        } else {
            None
        }
    }
    pub(super) fn should_stop(&self, job: &JobRow) -> bool {
        self.stop_reason(job).is_some()
    }
    /// Provider rate limiting: back off this batch's dispatch (1 s doubling
    /// to 60 s); a success resets it.
    fn rate_limited(&self) {
        #[cfg(test)]
        self.pauses.fetch_add(1, Ordering::SeqCst);
        let streak = self.busy_streak.fetch_add(1, Ordering::SeqCst).min(6);
        let wait = Duration::from_secs(1u64 << streak).min(Duration::from_secs(60));
        *self.pause_until.lock().expect("pause") = Some(tokio::time::Instant::now() + wait);
    }
    fn succeeded(&self) {
        self.busy_streak.store(0, Ordering::SeqCst);
    }
    /// Whether the batch is backing off after provider rate limiting.
    pub(super) fn paused(&self) -> bool {
        self.pause_until
            .lock()
            .expect("pause")
            .is_some_and(|until| tokio::time::Instant::now() < until)
    }
}

pub(super) struct LineRun {
    pub(super) store: Store,
    pub(super) engine: Engine,
    pub(super) repository: Arc<LineRepository>,
    pub(super) scheduler: Arc<Scheduler>,
    pub(super) job: JobRow,
    pub(super) endpoint: BatchEndpoint,
    pub(super) state: Arc<RunState>,
}
enum Retry {
    Started(Uuid),
    ConfirmedFailure,
    Unconfirmed,
}

impl LineRun {
    /// The probe slot bound of this run's lines (`Engine::probe_hold`).
    pub(super) fn probe_hold(&self) -> std::time::Duration {
        self.engine.probe_hold()
    }
    async fn emit_error(
        &self,
        records: &mpsc::Sender<Record>,
        line: i32,
        execution: Uuid,
        value: Value,
    ) {
        if !self.state.fenced.load(Ordering::SeqCst)
            && let Ok(record) = Record::encode(line, execution, true, &value)
        {
            let _ = records.send(record).await;
        }
    }

    async fn finished_error(
        &self,
        line: i32,
        execution: Uuid,
        status: Option<u16>,
        code: Option<&str>,
    ) -> bool {
        let result = self
            .state
            .fence
            .fail_line(&self.store, line, execution, status, code, true)
            .await;
        if !self.confirmed(line, execution, result) {
            return false;
        }
        #[cfg(test)]
        if self.state.soft_loss.swap(false, Ordering::SeqCst) {
            self.state.lost.store(true, Ordering::SeqCst);
        }
        crate::metrics::METRICS.observe_batch_lines("gateway", &self.job.provider, 0, 1);
        true
    }

    fn confirmed(&self, line: i32, execution: Uuid, result: FenceResult<bool>) -> bool {
        match self.state.written(result) {
            Ok(Some(true)) => return true,
            Ok(None) => {
                tracing::warn!(execution_id = %execution, line, "batch line outcome not recorded: the runner no longer holds the batch");
            }
            Ok(Some(false)) | Err(_) => {
                // Recovery interrupts unresolved attempts without re-execution.
                self.state.lost.store(true, Ordering::SeqCst);
                tracing::warn!(job_id = %self.job.id, "batch line state not confirmed");
            }
        }
        false
    }

    /// Run a claimed line (its `batch_lines` row exists) on its pinned route.
    pub(super) async fn run(
        &self,
        Head {
            n,
            raw,
            mut execution,
            ..
        }: Head,
        mut pin: Option<Pin>,
        records: mpsc::Sender<Record>,
        mut workers: Workers,
    ) {
        // The probe slot dispatch claimed for this line is freed if the line
        // ends before its attempt reaches the engine (an unparseable line, or
        // the task aborted); from the attempt on, the engine and `unpin` end it.
        let mut slot = pin.and_then(|pin| {
            pin.probe.map(|epoch| {
                crate::inference::ProbeTicket::held(
                    self.repository.clone(),
                    pin.deployment,
                    execution,
                    epoch,
                )
            })
        });
        let line = match lines::parse_work_line(&raw, self.endpoint) {
            Ok(l) => l,
            Err(_) => {
                let custom_id = lines::custom_id(&raw).unwrap_or_default();
                self.fail(
                    n,
                    &custom_id,
                    execution,
                    InferenceError::InvalidRequest,
                    &records,
                )
                .await;
                return;
            }
        };
        let model = line.request.model().to_owned();
        let mut attempt: i16 = 1;
        loop {
            let result = self
                .execute(line.request.clone(), execution, pin, line.plan)
                .await;
            if let Some(slot) = slot.take() {
                let _ = slot.into_epoch();
            }
            match result {
                Ok(response) => {
                    self.state.succeeded();
                    self.emit_success(&records, n, execution, &line.custom_id, &model, response)
                        .await;
                    return;
                }
                Err(e) if e.is_budget_denial() => {
                    // Admission refused: the line never ran. The batch stops.
                    self.state.budget.store(true, Ordering::SeqCst);
                    if self
                        .finished_error(n, execution, None, Some("budget_exceeded"))
                        .await
                    {
                        let value = lines::not_run_line(&line.custom_id, NotRun::BudgetExceeded);
                        self.emit_error(&records, n, execution, value).await;
                    }
                    return;
                }
                Err(e) if self.rescan_later(e, &mut pin, &mut workers).await => {}
                Err(e) => {
                    // Busy from an ungated refusal (`LineRepository::refuse_ungated`)
                    // never reached a provider: only a
                    // claimed attempt's Busy is upstream rate limiting, worth
                    // backing off the whole batch's dispatch for.
                    if e == InferenceError::Busy && pin.is_some() {
                        self.state.rate_limited();
                    }
                    // The line's pinned route could not be used when it was
                    // sent (its circuit open, or its probe slot held by
                    // another request or lost): nothing was sent, and the
                    // attempt is retried after its backoff like a failed one.
                    let cooling = matches!(e, InferenceError::RouteCoolingDown(_)) && pin.is_some();
                    let retryable = cooling
                        || matches!(
                            e,
                            InferenceError::Busy
                                | InferenceError::ScopeBusy
                                | InferenceError::UpstreamUnavailable
                                | InferenceError::Timeout
                        );
                    if retryable
                        && attempt <= self.job.retry_limit
                        && !self.state.should_stop(&self.job)
                    {
                        match self
                            .retry_failed_attempt(n, execution, attempt, pin, e)
                            .await
                        {
                            Retry::Started(next) => {
                                attempt += 1;
                                execution = next;
                                // The dispatch's probe slot was the first
                                // attempt's; a retry claims its own.
                                pin = pin.map(|pin| Pin { probe: None, ..pin });
                                continue;
                            }
                            Retry::ConfirmedFailure => {
                                let value = lines::failure_line(
                                    &line.custom_id,
                                    Some(execution),
                                    self.endpoint,
                                    e,
                                );
                                self.emit_error(&records, n, execution, value).await;
                            }
                            Retry::Unconfirmed => {}
                        }
                        return;
                    }
                    self.fail(n, &line.custom_id, execution, e, &records).await;
                    return;
                }
            }
        }
    }

    /// The guardrail step could not inspect the line now (no scan or compile
    /// slot, its policy not loaded): nothing was admitted or sent, so it is
    /// tried again after the error's delay, growing, without spending an
    /// attempt (`true` once the wait is over and the batch still runs). A
    /// line still refused after `GUARDRAIL_TRIES` fails with that error.
    async fn rescan_later(
        &self,
        e: InferenceError,
        pin: &mut Option<Pin>,
        workers: &mut Workers,
    ) -> bool {
        let tries = &mut workers.rescans;
        let limit = match rescan_cause(e) {
            Some(Rescan::Busy) => GUARDRAIL_TRIES,
            Some(Rescan::Spent) => SPENT_TRIES,
            None => return false,
        };
        if *tries >= limit || self.state.should_stop(&self.job) {
            return false;
        }
        let seconds = e.retry_after_seconds().unwrap_or(1) << (*tries).min(5);
        *tries += 1;
        // The worker permits go back while the line waits: a waiting line
        // never keeps other lines (of any batch) from running.
        workers.held = None;
        tokio::time::sleep(std::time::Duration::from_secs(
            seconds.min(GUARDRAIL_RETRY_CAP).into(),
        ))
        .await;
        // A batch stopped meanwhile (cancelled, expired, lost) runs nothing more.
        if self.state.should_stop(&self.job) || !workers.reacquire().await {
            return false;
        }
        // The dispatch's probe slot was the first try's.
        *pin = pin.map(|pin| Pin { probe: None, ..pin });
        true
    }

    async fn emit_success(
        &self,
        records: &mpsc::Sender<Record>,
        line: i32,
        execution: Uuid,
        custom_id: &str,
        model: &str,
        response: BatchResponse,
    ) {
        let encoded =
            lines::response_body(self.endpoint, execution, model, response).and_then(|body| {
                let value = lines::success_line(custom_id, Some(execution), body);
                Record::encode(line, execution, false, &value)
                    .map_err(|_| InferenceError::InvalidUpstream)
            });
        match encoded {
            Ok(record) => self.queue_success(records, record).await,
            Err(error) => self.fail(line, custom_id, execution, error, records).await,
        }
    }

    /// Only a durable failure can precede an explicit retry.
    async fn retry_failed_attempt(
        &self,
        line: i32,
        execution: Uuid,
        attempt: i16,
        pin: Option<Pin>,
        error: InferenceError,
    ) -> Retry {
        let (status, _) = crate::protocols::batch_line_error(false, error);
        let recorded = self
            .state
            .fence
            .fail_line(
                &self.store,
                line,
                execution,
                Some(status),
                Some(error.code()),
                false,
            )
            .await;
        if !self.confirmed(line, execution, recorded) {
            return Retry::Unconfirmed;
        }
        tokio::time::sleep(Duration::from_secs(1u64 << attempt)).await;
        let next = Uuid::now_v7();
        let route = pin.map(|pin| (pin.deployment, pin.max_concurrency));
        let mut recovered = None;
        while !self.state.should_stop(&self.job) {
            match self
                .scheduler
                .claim_retry(&self.state.fence, line, attempt, next, route)
                .await
            {
                Ok(RetryClaim::Started) => return Retry::Started(next),
                Ok(RetryClaim::Full) => tokio::time::sleep(POLL).await,
                Ok(claim @ (RetryClaim::Cancelled | RetryClaim::CancelledAfterCommit)) => {
                    self.state.cancel.store(true, Ordering::SeqCst);
                    if claim == RetryClaim::CancelledAfterCommit {
                        recovered = Some(UndispatchedRetry {
                            execution: next,
                            attempt: attempt + 1,
                            status,
                            code: error.code(),
                        });
                    }
                    break;
                }
                Err(FenceError::Stale) => {
                    self.state.lose();
                    return Retry::Unconfirmed;
                }
                Ok(RetryClaim::LineChanged) | Err(_) => break,
            }
        }
        let counted = self
            .state
            .fence
            .count_retry_failure(&self.store, line, execution, recovered)
            .await;
        if self.confirmed(line, execution, counted) {
            Retry::ConfirmedFailure
        } else {
            Retry::Unconfirmed
        }
    }

    /// Release route capacity separately from publication. The task holds
    /// its worker permits through the bounded channel send for backpressure.
    async fn queue_success(&self, records: &mpsc::Sender<Record>, record: Record) {
        let ready = self
            .state
            .fence
            .await_segment(&self.store, record.line, record.execution)
            .await;
        if !self.confirmed(record.line, record.execution, ready) {
            return;
        }
        #[cfg(test)]
        if self.state.soft_loss.swap(false, Ordering::SeqCst) {
            self.state.lost.store(true, Ordering::SeqCst);
        }
        if records.send(record).await.is_err() {
            self.state.lost.store(true, Ordering::SeqCst);
        }
    }

    async fn fail(
        &self,
        n: i32,
        custom_id: &str,
        execution: Uuid,
        error: InferenceError,
        records: &mpsc::Sender<Record>,
    ) {
        let (status, _) = crate::protocols::batch_line_error(false, error);
        if self
            .finished_error(n, execution, Some(status), Some(error.code()))
            .await
        {
            let value = lines::failure_line(custom_id, Some(execution), self.endpoint, error);
            self.emit_error(records, n, execution, value).await;
        }
    }

    /// Execute one attempt pinned to its route, with the route's priority
    /// hint (if any) scoped to this attempt.
    async fn execute(
        &self,
        mut request: BatchRequest,
        execution: Uuid,
        pin: Option<Pin>,
        plan: Option<lines::LinePlan>,
    ) -> Result<BatchResponse, InferenceError> {
        if let Some(deployment) = plan.map(|p| p.deployment).or(pin.map(|p| p.deployment)) {
            // `pin.is_some()` is the scheduler's own route-capacity claim
            // for this attempt, distinct from
            // merely carrying a plan: a creation-time plan alone (an
            // omitted-maximum line dispatched ungated because its pinned
            // route was briefly ineligible) must not count as gated.
            self.repository.pin(
                execution,
                deployment,
                plan.map(|p| p.price),
                pin.is_some(),
                pin.and_then(|p| p.probe.map(|epoch| (p.deployment, epoch))),
            );
        }
        // An omitted-maximum line is pinned at plan time to this one price,
        // which already resolved and validated the line against this exact
        // bound (`jobs::plan::scan`). Copy it onto the still-omitted request
        // before the engine ever sees it, so its pre-admission
        // `validate_chat_request` (and the real upstream attempt) judge the
        // same body planning did, not the raw client omission (#245). `0`
        // only for a plan written before this field existed; the omission
        // is left as-is for those already-queued lines, matching the prior
        // behavior, and admission still resolves the same price-derived
        // bound on its own (#215: the admission-pinned cap equals the
        // upstream cap either way).
        if let (Some(plan), BatchRequest::Chat(r)) = (plan, &mut request)
            && r.max_output_tokens.is_none()
            && plan.output > 0
        {
            r.max_output_tokens = Some(plan.output);
        }
        let attempt = self.execute_attempt(request, execution);
        let result = match pin.and_then(|p| p.priority.map(|priority| (p.deployment, priority))) {
            Some((deployment, priority)) => {
                crate::inference::scheduling::with_priority(deployment, priority, attempt).await
            }
            None => attempt.await,
        };
        self.repository.unpin(execution);
        result
    }

    async fn execute_attempt(
        &self,
        request: BatchRequest,
        execution: Uuid,
    ) -> Result<BatchResponse, InferenceError> {
        let principal = self.job.principal();
        match request {
            BatchRequest::Chat(r) => match self
                .engine
                .execute_protocol(principal, r, execution, self.endpoint.protocol())
                .await?
            {
                ProviderOutput::Complete(response) => Ok(BatchResponse::Chat(response)),
                ProviderOutput::Stream(_) => Err(InferenceError::InvalidUpstream),
            },
            BatchRequest::Embeddings(r) => self
                .engine
                .execute_embeddings(principal, r, execution)
                .await
                .map(BatchResponse::Embeddings),
        }
    }
}

/// Tries of a line the guardrail step could not inspect now, of one whose
/// custom-pattern budget ran out, and the longest wait between two
/// (seconds): about a minute in all, without the line's worker permits.
const GUARDRAIL_TRIES: u32 = 10;
const SPENT_TRIES: u32 = 2;
const GUARDRAIL_RETRY_CAP: u32 = 8;

/// Why a guardrail refusal of a line may be tried again.
#[derive(Debug, PartialEq, Eq)]
enum Rescan {
    /// Nothing could be scanned now (no scan or compile slot, the policy not
    /// loaded): the refusal says nothing about the line, and wrote no row.
    Busy,
    /// The custom-pattern budget ran out: a wall-clock budget, which a busy
    /// replica can exhaust too, so a line gets two more tries (each writes
    /// its decisions again).
    Spent,
}
fn rescan_cause(e: InferenceError) -> Option<Rescan> {
    use crate::guardrails::enforce::{Refusal, RefusalKind};
    match e {
        InferenceError::GuardrailUnavailable => Some(Rescan::Busy),
        InferenceError::Guardrail(Refusal {
            kind: RefusalKind::Unavailable,
            scanned,
            ..
        }) => Some(if scanned { Rescan::Spent } else { Rescan::Busy }),
        _ => None,
    }
}

/// A line's per-batch and process worker permits, given back while the line
/// waits to be inspected again, and how often it was.
pub(super) struct Workers {
    rescans: u32,
    held: Option<(
        tokio::sync::OwnedSemaphorePermit,
        tokio::sync::OwnedSemaphorePermit,
    )>,
    per_batch: Arc<tokio::sync::Semaphore>,
    pool: Arc<tokio::sync::Semaphore>,
}
impl Workers {
    pub(super) fn new(
        permits: (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
    ) -> Self {
        Self {
            rescans: 0,
            per_batch: Arc::clone(permits.0.semaphore()),
            pool: Arc::clone(permits.1.semaphore()),
            held: Some(permits),
        }
    }
    /// Both permits again, in dispatch's order; `false` if closed.
    async fn reacquire(&mut self) -> bool {
        let Ok(local) = Arc::clone(&self.per_batch).acquire_owned().await else {
            return false;
        };
        let Ok(global) = Arc::clone(&self.pool).acquire_owned().await else {
            return false;
        };
        self.held = Some((local, global));
        true
    }
}

#[cfg(test)]
mod guardrail_busy_tests {
    use super::*;
    use crate::guardrails::enforce::{Refusal, RefusalKind};
    use crate::guardrails::policy::{Category, DetectorKind};

    fn refusal(kind: RefusalKind, scanned: bool) -> InferenceError {
        InferenceError::Guardrail(Refusal {
            kind,
            rule_set_id: Uuid::nil(),
            rule_id: Uuid::nil(),
            category: Category::Custom,
            detector: DetectorKind::Pattern,
            scanned,
        })
    }

    #[test]
    fn only_a_refusal_that_could_not_finish_is_tried_again() {
        let busy = |e| rescan_cause(e) == Some(Rescan::Busy);
        assert!(busy(refusal(RefusalKind::Unavailable, false)));
        assert!(busy(InferenceError::GuardrailUnavailable));
        // A spent (wall-clock) budget gets a few tries; a block or a detector
        // failure is a decision.
        assert_eq!(
            rescan_cause(refusal(RefusalKind::Unavailable, true)),
            Some(Rescan::Spent)
        );
        assert_eq!(rescan_cause(refusal(RefusalKind::InputBlocked, true)), None);
        assert_eq!(
            rescan_cause(refusal(RefusalKind::DetectorFailed, false)),
            None
        );
        assert_eq!(rescan_cause(InferenceError::GuardrailThrottled(5)), None);
    }
}
