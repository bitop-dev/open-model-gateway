//! Whether an attempt may use a planned route now (#49): a half-open route
//! only as its probe, a drained one not at all; and what a request reports
//! when the routes it passed over leave nothing to attempt.
use super::*;

/// Whether an attempt may use a planned route now.
pub(crate) enum RouteUse {
    /// Yes; holding the route's probe slot, if it is half-open.
    Go(Option<ProbeTicket>),
    /// No: another request holds the half-open route's probe slot (or the
    /// circuit opened again) for about this many seconds. Nothing was sent.
    Busy(u32),
    /// No: an operator drained the route (`routing::drain`); it returns in
    /// about this many seconds when that is within the hour. Nothing was sent.
    Drained(Option<u32>),
}

/// The routes a request passed over without an attempt: the soonest return
/// of one cooling down, and whether (and about when) a drained one returns.
#[derive(Default, Clone, Copy)]
pub(crate) struct Passed {
    cooling: Option<u32>,
    drained: Option<Option<u32>>,
}
impl Passed {
    pub(crate) fn cool(&mut self, seconds: u32) {
        self.cooling = Some(self.cooling.map_or(seconds, |c| c.min(seconds)));
    }
    /// A drained route; `seconds` is its `Retry-After`, when within the hour.
    pub(crate) fn drain(&mut self, seconds: Option<u32>) {
        let seconds =
            seconds.filter(|s| *s <= crate::routing::drain::DRAIN_RETRY_AFTER_WITHIN_SECONDS);
        self.drained = Some(match (self.drained.flatten(), seconds) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        });
    }
}

/// An attempt that did not return: its telemetry, and whether it spent an
/// attempt. A route admission refused as drained since the plan was read
/// sent nothing and spends none; the next route is tried. `Break`: the
/// request's result.
pub(super) fn after(
    end: AttemptEnd,
    passed: &mut Passed,
    failed_over: &mut Option<InferenceError>,
) -> std::ops::ControlFlow<Result<ProviderOutput, InferenceError>, (RequestTelemetry, bool)> {
    use std::ops::ControlFlow::{Break, Continue};
    match end {
        AttemptEnd::Failover(telemetry, error) => {
            *failed_over = Some(error);
            Continue((telemetry, true))
        }
        AttemptEnd::Skipped(telemetry, seconds) => {
            passed.drain(seconds);
            Continue((telemetry, false))
        }
        AttemptEnd::Return(result) => Break(result),
    }
}

/// The error of a request whose plan ran out: the routes left could not be
/// used and none was attempted (cooling down, or drained by an operator), or
/// the attempts ran out.
pub(crate) fn exhausted(
    passed: Passed,
    attempts: usize,
    failed_over: Option<InferenceError>,
) -> InferenceError {
    match (passed.cooling, failed_over) {
        (Some(seconds), _) if attempts == 0 => InferenceError::RouteCoolingDown(
            passed.drained.flatten().map_or(seconds, |d| d.min(seconds)),
        ),
        (None, _) if attempts == 0 && passed.drained.is_some() => {
            InferenceError::RouteDrained(passed.drained.flatten())
        }
        // The last attempt failed over to routes none of which could be used:
        // its own error stands.
        (_, Some(error)) => error,
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
    live: bool,
) -> RouteUse {
    use crate::routing::{ProbeClaim, RouteState};
    let ticket = |epoch| ProbeTicket {
        repository: repository.clone(),
        deployment: id,
        holder,
        epoch: Some(epoch),
    };
    // A slot the caller claimed before a wait (a batch line, before its
    // route capacity) is renewed for this request's own bound; one that
    // ended meanwhile is no longer the caller's, and the route is not used.
    if let Some((held, epoch)) = plan.held_probe
        && held == id
    {
        return match repository.route_probe_renew(id, epoch, hold).await {
            Ok(true) => RouteUse::Go(Some(ticket(epoch))),
            Ok(false) => RouteUse::Busy(1),
            // Not known to be lost: the dropped ticket frees it (fenced).
            Err(_) => {
                drop(ticket(epoch));
                RouteUse::Busy(1)
            }
        };
    }
    // A route planned earlier in the request (a failover attempt) is taken
    // as it is now: its circuit may have opened or passed its cooldown since.
    let half_open = if live {
        match repository.route_state(id).await {
            Ok(RouteState::Closed) => false,
            Ok(RouteState::HalfOpen) => true,
            Ok(RouteState::Open(seconds)) => return RouteUse::Busy(seconds),
            // Drained since the plan: no new attempt on it.
            Ok(RouteState::Drained(seconds)) => return RouteUse::Drained(seconds),
            Err(_) => return RouteUse::Busy(1),
        }
    } else {
        plan.probes.contains(&id)
    };
    if !half_open {
        return RouteUse::Go(None);
    }
    match repository.route_probe_claim(id, holder, hold).await {
        Ok(ProbeClaim::Claimed(epoch)) => RouteUse::Go(Some(ticket(epoch))),
        Ok(ProbeClaim::Closed) => RouteUse::Go(None),
        Ok(ProbeClaim::Busy(seconds)) => RouteUse::Busy(seconds),
        Err(_) => RouteUse::Busy(1),
    }
}
