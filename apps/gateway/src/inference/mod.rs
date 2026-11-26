mod attempt;
pub mod audio;
pub mod client;
pub mod count;
mod deadline_stream;
pub mod dispatch;
mod embeddings;
pub mod error;
pub(crate) mod evidence;
mod hand_off;
pub mod images;
pub mod realtime;
pub mod repository;
mod request_telemetry;
pub mod scheduling;
pub mod types;
pub mod workload;
pub mod workload_types;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
type SharedPermit = Arc<Mutex<Option<OwnedSemaphorePermit>>>;

use futures_util::StreamExt;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{auth::Principal, providers::ProviderRegistry, telemetry};
use attempt::{AttemptEnd, RequestScope, record_routing_span};
use error::InferenceError;
use repository::{AttemptTelemetry, ExecutionFinish, ExecutionStart, InferenceRepository, Outcome};
use request_telemetry::*;
use types::{ApiProtocol, ChatEvent, ChatRequest, Deployment, ProviderOutput, Usage};

/// Opaque proof that a trace id came from this module's own
/// shared-admission-proven `log_trace_id` provenance (#98, #294): computed
/// only right after a shared-workspace admission has already committed for
/// this request (see the call site below), never for a personal workspace
/// or while telemetry is disabled. No crate-wide raw constructor: the
/// private `from_provenance` is reachable only from this module and its
/// submodules, never from `ops_log`, `leases`, `jobs` or any other module.
/// `as_str` is read-only and does not let a caller mint a new value from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTraceId(String);
impl LogTraceId {
    fn from_provenance(hex: String) -> Self {
        Self(hex)
    }
    /// Test fixtures only: compiled out of every production build, so this
    /// is never a production forgery path.
    #[cfg(test)]
    pub(crate) fn for_test(hex: &str) -> Self {
        Self(hex.to_owned())
    }
    /// Read-only: the existing hexadecimal value, for stderr diagnostics and
    /// the wire `traceId` field. Never lets a caller mint a new value.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy)]
pub struct EngineLimits {
    pub max_concurrent: usize,
    pub request_timeout: Duration,
    pub workloads: workload::WorkloadLimits,
    pub audio: audio::AudioLimits,
    /// Token counts this process admits per key per UTC minute (`count`).
    pub counts_per_key_per_minute: u32,
    /// Provider-backed token counts this process admits per provider
    /// connection credential per UTC minute (`count`).
    pub counts_per_connection_per_minute: u32,
    /// Token counts this process runs at once, from a pool separate from
    /// generation's `max_concurrent` (`count`).
    pub count_concurrency: usize,
}
impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 128,
            request_timeout: Duration::from_secs(120),
            workloads: workload::WorkloadLimits::default(),
            audio: audio::AudioLimits::default(),
            counts_per_key_per_minute: count::COUNTS_PER_KEY_PER_MINUTE,
            counts_per_connection_per_minute: count::COUNTS_PER_CONNECTION_PER_MINUTE,
            count_concurrency: count::COUNT_CONCURRENCY,
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    repository: Arc<dyn InferenceRepository>,
    registry: ProviderRegistry,
    capacity: Arc<Semaphore>,
    limits: EngineLimits,
    /// Realtime session limits (`Engine::with_realtime_limits`).
    realtime: realtime::RealtimeLimits,
    /// Bounded trace recorder (`Engine::with_telemetry`); None disables
    /// tracing, in which case the request path never allocates a
    /// RequestTelemetry context or hashes a request id.
    recorder: Option<telemetry::Recorder>,
    /// Token-count windows, concurrency and clock of this process (`count`).
    counts: Arc<count::Counts>,
}

impl Engine {
    /// How long a half-open route's probe slot is held at most: the
    /// request's own timeout and its settlement deadline.
    pub(crate) fn probe_hold(&self) -> Duration {
        self.limits.request_timeout + SETTLEMENT_DEADLINE
    }
    /// [`route_use`] for this engine's attempts: the probe slot it holds for
    /// `id`, or `None` (the wait noted in `cooling`) while another request
    /// holds it.
    async fn usable(
        &self,
        plan: &crate::routing::RoutePlan,
        id: Uuid,
        holder: Uuid,
        cooling: &mut Option<u32>,
    ) -> Option<Option<ProbeTicket>> {
        match route_use(&self.repository, plan, id, holder, self.probe_hold()).await {
            RouteUse::Go(probe) => Some(probe),
            RouteUse::Busy(seconds) => {
                *cooling = Some(cooling.map_or(seconds, |c| c.min(seconds)));
                None
            }
        }
    }
    pub fn new(
        repository: Arc<dyn InferenceRepository>,
        registry: ProviderRegistry,
        limits: EngineLimits,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            limits.max_concurrent > 0
                && limits.max_concurrent <= Semaphore::MAX_PERMITS
                && !limits.request_timeout.is_zero()
                && limits.workloads.validate()
                && limits.audio.validate()
                && limits.counts_per_key_per_minute > 0
                && limits.counts_per_connection_per_minute > 0
                && limits.count_concurrency > 0
                && limits.count_concurrency <= Semaphore::MAX_PERMITS,
            "invalid inference limits"
        );
        Ok(Self {
            repository,
            registry,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
            limits,
            realtime: realtime::RealtimeLimits::default(),
            recorder: None,
            counts: Arc::new(count::Counts::new(limits.count_concurrency)),
        })
    }

    /// Concurrency permits not held by a request, a session or a detached
    /// write that still carries one.
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) fn available_capacity(&self) -> usize {
        self.capacity.available_permits()
    }

    /// Attaches the bounded trace recorder (#98). None disables tracing for
    /// every request through this engine.
    pub fn with_telemetry(mut self, recorder: Option<telemetry::Recorder>) -> Self {
        self.recorder = recorder;
        self
    }

    /// The same engine (adapters, limits, deadlines) with another repository
    /// and its own capacity, for background work such as gateway-run batch
    /// lines (`jobs::runner`), whose admission differs from interactive
    /// requests but whose execution path must not.
    pub fn with_repository(
        &self,
        repository: Arc<dyn InferenceRepository>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            repository,
            registry: self.registry.clone(),
            capacity: Arc::new(Semaphore::new(
                max_concurrent.clamp(1, Semaphore::MAX_PERMITS),
            )),
            limits: self.limits,
            realtime: self.realtime,
            // Background work (gateway-run batch lines) is out of scope for
            // interactive-generation tracing: never wrapped in telemetry,
            // same as every other non-interactive admission path.
            recorder: None,
            counts: self.counts.clone(),
        }
    }

    pub async fn execute(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<ProviderOutput, InferenceError> {
        self.execute_protocol(principal, request, request_id, ApiProtocol::ChatCompletions)
            .await
    }

    pub async fn execute_protocol(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput, InferenceError> {
        let permit = Arc::new(Mutex::new(Some(
            self.capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| crate::metrics::capacity_denied())?,
        )));
        let started = Instant::now();
        let deadline = started + self.limits.request_timeout;
        let labels = client::current();
        // Reserved before anything else: routing/admission/attempt/
        // settlement spans all name this as `parent`. Export never happens
        // without a later shared-workspace admission commit (#98).
        let mut telemetry = RequestTelemetry::new(
            self.recorder.as_ref(),
            request_id,
            operation_of(protocol),
            request.stream,
        );
        let routing_start = telemetry.active().then(telemetry::now_unix_nanos);
        let deployments = timeout_at(
            deadline,
            self.repository
                .candidate_deployments(&principal, &request.model, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let (candidates, refused) = self.eligible_candidates(deployments, &request, protocol);
        if candidates.is_empty() {
            if refused.is_empty() {
                return Err(InferenceError::Unsupported);
            }
            return self
                .refuse(&principal, &request, &refused, request_id, deadline)
                .await;
        }
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan_probing(&principal, &request.model, &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if !(1..=3).contains(&plan.max_attempts) {
            return Err(InferenceError::Configuration);
        }
        record_routing_span(&mut telemetry, routing_start, protocol);
        let streamed = request.stream;
        let scope = RequestScope {
            principal,
            request: &request,
            request_id,
            deadline,
            labels: &labels,
            permit: &permit,
            plan: &plan,
            protocol,
            streamed,
        };
        let (mut attempt, mut cooling) = (0, None);
        for id in &plan.deployment_ids {
            if attempt == plan.max_attempts {
                break;
            }
            let deployment = match candidates.iter().find(|d| d.id == *id) {
                Some(deployment) => deployment,
                None => {
                    // An early return must still finish telemetry
                    // explicitly, exactly like every other terminal path of
                    // an attempt (`Engine::attempt`), so Drop never
                    // mislabels a real denial `cancelled` with no
                    // `error.type`.
                    telemetry.finish(
                        telemetry::Outcome::Error,
                        Some(InferenceError::Configuration),
                    );
                    return Err(InferenceError::Configuration);
                }
            };
            let adapter = match self.registry.get(&deployment.provider) {
                Some(adapter) => adapter,
                None => {
                    telemetry.finish(
                        telemetry::Outcome::Error,
                        Some(InferenceError::Configuration),
                    );
                    return Err(InferenceError::Configuration);
                }
            };
            let Some(probe) = self.usable(&plan, *id, request_id, &mut cooling).await else {
                continue;
            };
            telemetry = match self
                .attempt(&scope, telemetry, attempt, deployment, adapter, probe)
                .await
            {
                AttemptEnd::Failover(telemetry) => telemetry,
                AttemptEnd::Return(result) => return result,
            };
            attempt += 1;
        }
        let error = exhausted(cooling, attempt);
        telemetry.finish(telemetry::Outcome::Error, Some(error));
        Err(error)
    }
}

/// The configured upstream model id as stored (1..=512 characters), else none.
fn upstream_snapshot(model: &str) -> Option<String> {
    (1..=512)
        .contains(&model.chars().count())
        .then(|| model.to_owned())
}

fn valid_usage(usage: Usage) -> bool {
    usage.meters.is_none_or(|m| m.validate().is_ok())
        && usage.provider_cost_microusd.is_none_or(|n| n >= 0)
        && valid_token_usage(usage)
}
fn valid_token_usage(usage: Usage) -> bool {
    [usage.input_tokens, usage.output_tokens]
        .into_iter()
        .flatten()
        .all(|n| n <= i64::MAX as u64)
        && u128::from(usage.input_tokens.unwrap_or(0))
            + u128::from(usage.output_tokens.unwrap_or(0))
            <= i64::MAX as u128
        && usage.billing.is_none_or(|billing| {
            billing.input_lower_bound().is_ok_and(|n| {
                u128::from(n) + u128::from(usage.output_tokens.unwrap_or(0)) <= i64::MAX as u128
            }) && billing.validate().is_ok()
                && match (billing.total_input_tokens, usage.input_tokens) {
                    (Some(total), Some(raw)) => total >= raw,
                    _ => true,
                }
        })
}

/// Settles one attempt's durable write (wrapping its real phase boundaries
/// as a `settlement` span, via [`begin_phase_group`]/[`end_phase_group`])
/// and records its `upstream.attempt` span, regardless of whether the
/// settlement write itself succeeded. Returns settlement's own result
/// unchanged so the caller can decide whether to fail over or return.
#[allow(clippy::too_many_arguments)]
async fn settle_attempt(
    telemetry: &mut RequestTelemetry,
    guard: &mut ExecutionGuard,
    outcome: Outcome,
    error: Option<InferenceError>,
    attempt_number: usize,
    attempt_wall_start: Option<u64>,
    attempt_id: Uuid,
    provider: &str,
    streamed: bool,
) -> Result<(), InferenceError> {
    let telemetry_outcome = match outcome {
        Outcome::Succeeded => telemetry::Outcome::Completed,
        Outcome::Failed => telemetry::Outcome::Error,
        Outcome::Cancelled => telemetry::Outcome::Cancelled,
    };
    let usage = guard.usage;
    let timings = guard.telemetry_now();
    // Captured here, before settlement's own (possibly slow, up to its 3s
    // timeout) write: `upstream.attempt` ends at the terminal upstream
    // outcome, never inflated by settlement's own duration. `settlement`
    // is recorded separately below with its own real
    // start/end from `begin_phase_group`/`end_phase_group`.
    let attempt_end = telemetry::now_unix_nanos();
    let group = begin_phase_group(telemetry);
    let buffer = group.as_ref().map(|(b, _, _)| b.clone());
    let result = guard.finish(outcome, error, buffer).await;
    end_phase_group(
        telemetry,
        group,
        telemetry::SpanName::Settlement,
        attempt_number,
    );
    if let Some(start) = attempt_wall_start
        && let Some(span) = attempt_span(
            telemetry,
            start,
            attempt_end,
            attempt_number,
            provider,
            streamed,
            attempt_id,
            telemetry_outcome,
            error,
            usage,
            &timings,
        )
    {
        telemetry.record(span);
    }
    result
}

/// How long a settlement may take: the one the request waits for and the
/// detached one of a cancelled request alike.
pub(crate) const SETTLEMENT_DEADLINE: Duration = Duration::from_secs(3);

/// Whether an attempt may use a planned route now.
pub(crate) enum RouteUse {
    /// Yes; holding the route's probe slot, if it is half-open.
    Go(Option<ProbeTicket>),
    /// No: another request holds the half-open route's probe slot (or the
    /// circuit opened again) for about this many seconds. Nothing was sent.
    Busy(u32),
}

/// The error of a request whose plan ran out: the routes left were other
/// requests' probes and none was attempted (cooling down), or attempts ran out.
pub(crate) fn exhausted(cooling: Option<u32>, attempts: usize) -> InferenceError {
    match cooling {
        Some(seconds) if attempts == 0 => InferenceError::RouteCoolingDown(seconds),
        _ => InferenceError::Busy,
    }
}

/// A half-open route of the plan is used only by the request that claims its
/// probe slot, for at most `hold` (the request's timeout and settlement
/// deadline); every other route as before. A claim that cannot be made
/// (storage) is treated as busy: the route is not used unclaimed.
pub(crate) async fn route_use(
    repository: &Arc<dyn InferenceRepository>,
    plan: &crate::routing::RoutePlan,
    id: Uuid,
    holder: Uuid,
    hold: Duration,
) -> RouteUse {
    use crate::routing::ProbeClaim;
    let ticket = |epoch| ProbeTicket {
        repository: repository.clone(),
        deployment: id,
        holder,
        epoch: Some(epoch),
    };
    if let Some((held, epoch)) = plan.held_probe
        && held == id
    {
        return RouteUse::Go(Some(ticket(epoch)));
    }
    if !plan.probes.contains(&id) {
        return RouteUse::Go(None);
    }
    match repository.route_probe_claim(id, holder, hold).await {
        Ok(ProbeClaim::Claimed(epoch)) => RouteUse::Go(Some(ticket(epoch))),
        Ok(ProbeClaim::Closed) => RouteUse::Go(None),
        Ok(ProbeClaim::Busy(seconds)) => RouteUse::Busy(seconds),
        Err(_) => RouteUse::Busy(1),
    }
}

/// A probe slot an attempt holds before its `ExecutionGuard` owns it. If the
/// attempt ends first (refused at admission, or the request dropped while it
/// was admitted), dropping the ticket frees the slot for the next request;
/// the guard takes the epoch over once it exists ([`Self::into_epoch`]).
pub(crate) struct ProbeTicket {
    repository: Arc<dyn InferenceRepository>,
    deployment: Uuid,
    holder: Uuid,
    epoch: Option<i64>,
}
impl ProbeTicket {
    /// The slot's epoch, now the guard's to end.
    pub(crate) fn into_epoch(mut self) -> Option<i64> {
        self.epoch.take()
    }
}
impl Drop for ProbeTicket {
    fn drop(&mut self) {
        if let Some(epoch) = self.epoch.take() {
            let _ = end_probe_in_background(
                self.repository.clone(),
                self.deployment,
                epoch,
                false,
                self.holder,
            );
        }
    }
}

/// End a probe slot in the background, bounded like the advisory health
/// write: close the circuit (a stream's first content) or free the slot.
/// Both are fenced by `epoch`, so a slot that ended otherwise (a decision,
/// its expiry, a newer claim) is left alone; a write that never lands leaves
/// the slot to end at its expiry.
fn end_probe_in_background(
    repository: Arc<dyn InferenceRepository>,
    deployment: Uuid,
    epoch: i64,
    close: bool,
    execution_id: Uuid,
) -> Option<tokio::task::JoinHandle<()>> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    Some(runtime.spawn(async move {
        let write = async {
            if close {
                repository.route_probe_close(deployment, epoch).await
            } else {
                repository.route_probe_release(deployment, epoch).await
            }
        };
        if !matches!(timeout(Duration::from_millis(250), write).await, Ok(Ok(()))) {
            tracing::warn!(execution_id = %execution_id, "routing probe update failed");
        }
    }))
}
struct ExecutionGuard {
    repository: Arc<dyn InferenceRepository>,
    id: Uuid,
    deployment_id: Uuid,
    started: Instant,
    /// Upstream dispatch (after admission): the origin of attempt timings.
    dispatched: Instant,
    usage: Usage,
    telemetry: AttemptTelemetry,
    finished: bool,
    health_observation: bool,
    /// The epoch of the half-open route's probe slot this attempt holds
    /// (`routing::claim_probe`); `None` once freed or when it holds none.
    probe: Option<i64>,
    /// The background close of the probe's circuit at its first content: the
    /// health write waits for it, so a later failure is an ordinary one.
    probe_closing: Option<tokio::task::JoinHandle<()>>,
    _permit: SharedPermit,
    /// The request's correlation id (#98), for the reconciliation log lines
    /// below only: `Some` only once a shared-workspace admission has
    /// already committed for this request, `None` for a personal workspace
    /// (never logged) or while telemetry is disabled. Never affects
    /// accounting; `x-request-id` is unchanged.
    log_trace_id: Option<LogTraceId>,
    #[cfg(test)]
    diagnostics: Option<crate::db::diagnostics::Handle>,
}
fn millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}
impl ExecutionGuard {
    /// Created right after durable admission, immediately before dispatch.
    fn new(
        repository: Arc<dyn InferenceRepository>,
        id: Uuid,
        deployment_id: Uuid,
        started: Instant,
        permit: SharedPermit,
        log_trace_id: Option<LogTraceId>,
    ) -> Self {
        Self {
            repository,
            id,
            deployment_id,
            started,
            dispatched: Instant::now(),
            usage: Usage::default(),
            telemetry: AttemptTelemetry::default(),
            finished: false,
            health_observation: true,
            probe: None,
            probe_closing: None,
            _permit: permit,
            log_trace_id,
            #[cfg(test)]
            diagnostics: crate::db::diagnostics::current(),
        }
    }
    /// Streams: the first delta carrying text or a tool call sets time to first token.
    /// It is also a probe's first content: the route is serving, so its circuit
    /// closes at once (`routing::close_probe`). An event without content (a
    /// role, a start or a keep-alive) does neither.
    fn first_token(&mut self, text: Option<&str>, tool_calls: &[types::ToolCallDelta]) {
        if self.telemetry.time_to_first_token_ms.is_none()
            && (text.is_some_and(|t| !t.is_empty()) || !tool_calls.is_empty())
        {
            self.telemetry.time_to_first_token_ms = Some(millis(self.dispatched.elapsed()));
            if let Some(epoch) = self.probe.take() {
                self.probe_closing = self.end_probe(epoch, true);
            }
        }
    }
    /// End this attempt's probe slot (`end_probe_in_background`).
    fn end_probe(&self, epoch: i64, close: bool) -> Option<tokio::task::JoinHandle<()>> {
        end_probe_in_background(
            self.repository.clone(),
            self.deployment_id,
            epoch,
            close,
            self.id,
        )
    }
    /// Telemetry as of now: generation time ends when the attempt finishes.
    fn telemetry_now(&self) -> AttemptTelemetry {
        AttemptTelemetry {
            generation_ms: self
                .telemetry
                .generation_ms
                .or(Some(millis(self.dispatched.elapsed()))),
            ..self.telemetry
        }
    }
    fn record(&self, outcome: Outcome, error: Option<InferenceError>) -> ExecutionFinish {
        ExecutionFinish {
            id: self.id,
            outcome,
            error,
            usage: self.usage,
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }
    }
    async fn finish(
        &mut self,
        outcome: Outcome,
        error: Option<InferenceError>,
        phases: Option<Arc<Mutex<telemetry::PhaseBuffer>>>,
    ) -> Result<(), InferenceError> {
        // Marked before the write starts: from here the settlement write is the
        // only owner of this attempt's accounting, and `Drop` never settles it.
        self.finished = true;
        let result = self.settle_to_completion(outcome, error, phases).await;
        // The probe's circuit closed at its first content before this result
        // is recorded (bounded: the close is).
        if let Some(closing) = self.probe_closing.take() {
            let _ = closing.await;
        }
        // Health is advisory; a failed health write never discards a durably accounted response.
        if self.health_observation
            && outcome != Outcome::Cancelled
            && !matches!(
                timeout(
                    Duration::from_millis(250),
                    self.repository.route_result(self.deployment_id, error)
                )
                .await,
                Ok(Ok(()))
            )
        {
            tracing::warn!(execution_id=%self.id, trace_id = self.log_trace_id.as_ref().map(LogTraceId::as_str), "routing health update failed");
        }
        // A probe the health write decided has no slot left; one it did not
        // decide (no health signal, cancelled, or a write that failed) frees it.
        if let Some(epoch) = self.probe.take() {
            let _ = self.end_probe(epoch, false);
        }
        result
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if let Some(epoch) = self.probe.take() {
            let _ = self.end_probe(epoch, false);
        }
        if self.finished {
            return;
        }
        let repository = self.repository.clone();
        let record = self.record(Outcome::Cancelled, None);
        let telemetry = self.telemetry_now();
        let log_trace_id = self.log_trace_id.clone();
        // Network work is dropped synchronously. Only best-effort accounting is detached.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let execution_id = record.id;
            // Construct only the future here; its unchanged deadline starts inside the owner task.
            let settlement = async move {
                timeout(
                    SETTLEMENT_DEADLINE,
                    repository.finish_attempt(&record, &telemetry),
                )
                .await
            };
            let settlement = crate::db::accounting_failure::cancellation_settlement(settlement);
            #[cfg(test)]
            let settlement = crate::db::diagnostics::settlement(
                self.diagnostics.clone(),
                crate::db::diagnostics::Stage::CancellationSettlement,
                settlement,
            );
            runtime.spawn(async move {
                let finished = settlement.await;
                if finished.is_err() {
                    crate::metrics::METRICS.observe_settlement(crate::metrics::Settlement::Held);
                }
                if !matches!(finished, Ok(Ok(()))) {
                    tracing::error!(execution_id = %execution_id, trace_id = log_trace_id.as_ref().map(LogTraceId::as_str), "cancelled inference finalization failed; reconciliation required");
                    // Cancellation never carries a trace id (#294): only the
                    // explicit `finish` path above is generation-finalization-proven.
                    crate::ops_log::emit(crate::ops_log::OpsEvent::FinalizationNeedsReconciliation {
                        finalizer: crate::ops_log::Finalizer::GenerationCancel,
                        trace: None,
                    });
                }
            });
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
pub(crate) mod admission_validation_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod count_db_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod settlement_disconnect_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod steering_tests;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "integration-tests"))]
mod unsent_tests;
