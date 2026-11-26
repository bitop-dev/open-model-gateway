//! Provider-independent routing over an already authorized, capability-filtered candidate set.
//! No network calls; health is a passive observation, not a guarantee.
//!
//! A route whose circuit has opened is closed to traffic until its cooldown
//! passes; it is then half-open, and one real request decides whether
//! traffic resumes (the route's probe slot, `claim_probe`). There are no
//! synthetic probes: every upstream attempt has its own reservation, and no
//! installation-wide scope exists to bill one to.

use std::collections::HashSet;

use gateway_datetime_compat::UtcInstant;
use rand::{
    SeedableRng,
    distr::{Distribution, weighted::WeightedIndex},
    rngs::StdRng,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    auth::Principal,
    inference::{error::InferenceError, repository::route_lookup_storage, types::Deployment},
    store::Store,
};

/// Reject oversized configurations rather than silently hiding eligible routes.
pub const MAX_CANDIDATES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutePlan {
    pub deployment_ids: Vec<Uuid>,
    pub max_attempts: usize,
    pub allow_ambiguous_failover: bool,
    /// Half-open routes of the plan: an attempt on one must first claim its
    /// probe slot (`claim_probe`). Only a probing plan (`plan_probing`) has any.
    pub probes: Vec<Uuid>,
    /// A probe slot the caller already holds for one route (a batch line
    /// claims it before its route capacity), with its epoch.
    pub held_probe: Option<(Uuid, i64)>,
}

impl Default for RoutePlan {
    fn default() -> Self {
        Self::single(Vec::new())
    }
}

impl RoutePlan {
    /// Preserve repository order, without opting in to another provider attempt.
    pub fn single(deployment_ids: Vec<Uuid>) -> Self {
        Self {
            deployment_ids,
            max_attempts: 1,
            allow_ambiguous_failover: false,
            probes: Vec::new(),
            held_probe: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct RoutingPolicy {
    pub strategy: String,
    pub max_attempts: i32,
    pub allow_ambiguous_failover: bool,
    pub failure_threshold: i32,
    pub cooldown_seconds: i32,
    pub required_residency: Option<String>,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            strategy: "priority".into(),
            max_attempts: 1,
            allow_ambiguous_failover: false,
            failure_threshold: 3,
            cooldown_seconds: 30,
            required_residency: None,
        }
    }
}

impl RoutingPolicy {
    pub fn validate(&self) -> Result<(), InferenceError> {
        if !matches!(self.strategy.as_str(), "priority" | "weighted")
            || !(1..=3).contains(&self.max_attempts)
            || self.failure_threshold < 1
            || !(1..=3600).contains(&self.cooldown_seconds)
            || self
                .required_residency
                .as_deref()
                .is_some_and(|v| !valid_residency(v) || v == "unspecified")
        {
            return Err(InferenceError::Configuration);
        }
        Ok(())
    }
}

#[derive(Clone, sqlx::FromRow)]
struct Candidate {
    deployment_id: Uuid,
    priority: i32,
    weight: i32,
    residency: String,
    operator_disabled: bool,
    circuit_open: bool,
    /// Whole seconds until an open circuit closes (0 when closed).
    cooldown_remaining_seconds: i32,
    /// The circuit opened and its cooldown has passed: one probe decides.
    half_open: bool,
    /// Whole seconds until a held probe slot ends on its own (0 when free).
    probe_held_seconds: i32,
    /// An operator drain applies now (`routing::drain`): no new attempt.
    drained: bool,
    /// Seconds until the drain ends, when it ends at all.
    drain_ends_in: Option<f64>,
}

/// The caller owns current authorization and capability checks. The database query
/// additionally fences candidate IDs to live workspace authorization and model. Planning does
/// not reserve a half-open probe or leak a lease if the request is cancelled.
/// Routing configuration and passive health are platform-global resource state.
///
/// For a caller that records no health (a token count, a native job's
/// submission, a realtime session): it cannot decide a probe, and a
/// half-open route stays eligible to it, as an expired cooldown always was.
pub async fn plan(
    store: &Store,
    principal: &Principal,
    model: &str,
    candidates: &[Deployment],
    request_id: Uuid,
) -> Result<RoutePlan, InferenceError> {
    let inputs = RouteInputs::load(store, principal, model, candidates).await?;
    order_candidates(
        &inputs.policy,
        inputs.rows,
        weighted_order_seed(principal, model, request_id),
        false,
    )
}

/// [`plan`] for a caller whose attempts record health (`record_result`): a
/// half-open route is in the plan only while its probe slot is free, listed
/// in `RoutePlan::probes`, and an attempt on it must claim the slot first.
/// One whose slot another request holds is cooling down.
pub async fn plan_probing(
    store: &Store,
    principal: &Principal,
    model: &str,
    candidates: &[Deployment],
    request_id: Uuid,
) -> Result<RoutePlan, InferenceError> {
    let inputs = RouteInputs::load(store, principal, model, candidates).await?;
    order_candidates(
        &inputs.policy,
        inputs.rows,
        weighted_order_seed(principal, model, request_id),
        true,
    )
}

/// Reuse the read-only inputs within a batch scan. Dispatch loads its own
/// inputs and uses the same ordering with the current eligible routes.
pub(crate) struct RouteInputs {
    policy: RoutingPolicy,
    rows: Vec<Candidate>,
}

impl RouteInputs {
    pub(crate) async fn load(
        store: &Store,
        principal: &Principal,
        model: &str,
        candidates: &[Deployment],
    ) -> Result<Self, InferenceError> {
        if candidates.len() > MAX_CANDIDATES {
            return Err(InferenceError::Configuration);
        }
        let ids: Vec<_> = candidates.iter().map(|d| d.id).collect();
        validate_ids(&ids)?;
        let (policy, rows) = if store.caches.enabled() {
            cached_inputs(store, principal, model, &ids).await?
        } else {
            live_inputs(store, principal, model, &ids).await?
        };
        Ok(Self { policy, rows })
    }

    fn subset(&self, ids: &HashSet<Uuid>) -> Vec<Candidate> {
        self.rows
            .iter()
            .filter(|row| ids.contains(&row.deployment_id))
            .cloned()
            .collect()
    }

    pub(crate) fn plan_for(
        &self,
        principal: &Principal,
        model: &str,
        request_id: Uuid,
        ids: &HashSet<Uuid>,
    ) -> Result<RoutePlan, InferenceError> {
        order_candidates(
            &self.policy,
            self.subset(ids),
            weighted_order_seed(principal, model, request_id),
            false,
        )
    }

    pub(crate) fn is_cooling(&self, id: Uuid) -> bool {
        let candidates = self
            .rows
            .iter()
            .filter(|row| row.deployment_id == id)
            .cloned()
            .collect();
        matches!(
            eligible_candidates(&self.policy, candidates, false),
            Err(InferenceError::RouteCoolingDown(_) | InferenceError::RouteDrained(_))
        )
    }

    /// Estimate the scheduler's first eligible route for an unpinned line.
    /// Weighted routing only needs the first draw.
    pub(crate) fn first_route(
        &self,
        principal: &Principal,
        model: &str,
        request_id: Uuid,
        ids: &HashSet<Uuid>,
    ) -> Result<Uuid, InferenceError> {
        let candidates = eligible_candidates(&self.policy, self.subset(ids), false)?;
        let index = if self.policy.strategy == "weighted" {
            let mut rng = StdRng::from_seed(weighted_order_seed(principal, model, request_id));
            weighted_index(&candidates, &mut rng)?
        } else {
            0
        };
        Ok(candidates[index].deployment_id)
    }
}

fn weighted_order_seed(principal: &Principal, model: &str, request_id: Uuid) -> [u8; 32] {
    let mut seed = Sha256::new();
    seed.update(principal.workspace_id.as_bytes());
    seed.update(request_id.as_bytes());
    seed.update(model.as_bytes());
    seed.finalize().into()
}

/// Planning inputs read live: the model's routing policy (as the workspace
/// sees it) and the candidates' routing rows and passive health, in input
/// order, fenced to live catalog eligibility.
async fn live_inputs(
    store: &Store,
    principal: &Principal,
    model: &str,
    ids: &[Uuid],
) -> Result<(RoutingPolicy, Vec<Candidate>), InferenceError> {
    let policy = sqlx::query_as::<_, RoutingPolicy>(POLICY_SQL)
        .bind(principal.workspace_id)
        .bind(model)
        .fetch_optional(&store.pool)
        .await
        .map_err(route_lookup_storage)?
        .unwrap_or_default();
    let sql = format!(
        r#"SELECT d.id AS deployment_id, COALESCE(r.priority,0) AS priority,
                  COALESCE(r.weight,1) AS weight, COALESCE(r.residency,'unspecified') AS residency,
                  false AS operator_disabled,
                  COALESCE(h.open_until > statement_timestamp(),false) AS circuit_open,
                  CASE WHEN h.open_until > statement_timestamp()
                       THEN LEAST(86400, GREATEST(1, CEIL(EXTRACT(EPOCH FROM h.open_until - statement_timestamp()))))::int
                       ELSE 0 END AS cooldown_remaining_seconds,
                  COALESCE(h.open_until <= statement_timestamp(),false) AS half_open,
                  CASE WHEN h.probe_until > statement_timestamp()
                       THEN LEAST(86400, GREATEST(1, CEIL(EXTRACT(EPOCH FROM h.probe_until - statement_timestamp()))))::int
                       ELSE 0 END AS probe_held_seconds,
                  {drained} AS drained, {ends} AS drain_ends_in
           FROM unnest($3::uuid[]) WITH ORDINALITY AS input(id, position)
           JOIN deployments d ON d.id=input.id
           JOIN models m ON m.id=d.model_id
           JOIN provider_connections p ON p.id=d.provider_connection_id
           LEFT JOIN deployment_routing r ON r.deployment_id=d.id
           LEFT JOIN deployment_health h ON h.deployment_id=d.id
           WHERE m.public_name=$2 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled
           ORDER BY input.position"#,
        drained = drain::ROUTE_DRAINED,
        ends = drain::DRAIN_ENDS_IN,
    );
    let rows = sqlx::query_as::<_, Candidate>(&sql)
        .bind(principal.workspace_id)
        .bind(model)
        .bind(ids)
        .fetch_all(&store.pool)
        .await
        .map_err(route_lookup_storage)?;
    Ok((policy, rows))
}

const POLICY_SQL: &str = r#"SELECT r.strategy, r.max_attempts, r.allow_ambiguous_failover,
                  3 AS failure_threshold, 30 AS cooldown_seconds, r.required_residency
           FROM routing_policies r JOIN models m ON m.id=r.model_id
           WHERE workspace_model_allowed($1,m.id) AND m.public_name=$2"#;

/// [`live_inputs`] through the per-replica caches: the routing snapshot of
/// (workspace, model) while the catalog and access versions are unchanged,
/// and passive health read at most once per [`crate::cache::HEALTH_TTL`]
/// (this replica's own results invalidate it at once). Candidates missing
/// from the snapshot are dropped, as the live join drops them.
async fn cached_inputs(
    store: &Store,
    principal: &Principal,
    model: &str,
    ids: &[Uuid],
) -> Result<(RoutingPolicy, Vec<Candidate>), InferenceError> {
    use crate::cache::{Lookup, RouteSnapshot};
    let caches = &store.caches;
    let key = (principal.workspace_id, model.to_owned());
    let snapshot = match caches.routes.get(&caches.versions, &key) {
        Lookup::Hit(snapshot) => snapshot,
        lookup => {
            let policy = sqlx::query_as::<_, RoutingPolicy>(POLICY_SQL)
                .bind(principal.workspace_id)
                .bind(model)
                .fetch_optional(&store.pool)
                .await
                .map_err(route_lookup_storage)?;
            let rows: Vec<(Uuid, i32, i32, String)> = sqlx::query_as(
                r#"SELECT d.id, COALESCE(r.priority,0), COALESCE(r.weight,1), COALESCE(r.residency,'unspecified')
                   FROM models m JOIN deployments d ON d.model_id=m.id
                   JOIN provider_connections p ON p.id=d.provider_connection_id
                   LEFT JOIN deployment_routing r ON r.deployment_id=d.id
                   WHERE m.public_name=$2 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled
                   LIMIT 257"#,
            )
            .bind(principal.workspace_id)
            .bind(model)
            .fetch_all(&store.pool)
            .await
            .map_err(route_lookup_storage)?;
            if rows.len() > MAX_CANDIDATES {
                return Err(InferenceError::Configuration);
            }
            let snapshot = std::sync::Arc::new(RouteSnapshot {
                policy,
                rows: rows
                    .into_iter()
                    .map(|(id, priority, weight, residency)| (id, (priority, weight, residency)))
                    .collect(),
            });
            if let Lookup::Miss(stamp) = lookup {
                caches.routes.insert(key, snapshot.clone(), stamp);
            }
            snapshot
        }
    };
    let eligible: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| snapshot.rows.contains_key(id))
        .collect();
    let (mut health, missing) = caches.health(&eligible);
    if !missing.is_empty() {
        let fresh = load_health(store, &missing).await?;
        health.extend(fresh.iter().copied());
        caches.store_health(fresh);
    }
    let now = std::time::Instant::now();
    let rows = eligible
        .into_iter()
        .map(|id| {
            let (priority, weight, residency) = snapshot.rows[&id].clone();
            let entry = health.get(&id);
            let until = entry.and_then(|h| h.open_until);
            let open = until.filter(|until| *until > now);
            let probe = entry
                .and_then(|h| h.probe_until)
                .filter(|until| *until > now);
            let seconds = |until: std::time::Instant| {
                (until - now).as_secs_f64().ceil().clamp(1.0, 86_400.0) as i32
            };
            Candidate {
                deployment_id: id,
                priority,
                weight,
                residency,
                operator_disabled: false,
                circuit_open: open.is_some(),
                cooldown_remaining_seconds: open.map_or(0, seconds),
                half_open: until.is_some() && open.is_none(),
                probe_held_seconds: probe.map_or(0, seconds),
                drained: entry.is_some_and(|h| h.drained_at(now)),
                drain_ends_in: entry
                    .and_then(|h| h.drain_until)
                    .map(|until| until.saturating_duration_since(now).as_secs_f64()),
            }
        })
        .collect();
    Ok((snapshot.policy.clone().unwrap_or_default(), rows))
}

/// Passive health of `ids` as read now, for the caches: an id without a row
/// is a route that never failed.
async fn load_health(
    store: &Store,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, crate::cache::HealthEntry)>, InferenceError> {
    use crate::cache::HealthEntry;
    let loaded = std::time::Instant::now();
    // (deployment, failures, seconds until the circuit closes, seconds
    // since the last observation, seconds the probe slot is held, openings,
    // drained, seconds until the drain ends)
    type HealthRow = (
        Uuid,
        Option<i32>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<i32>,
        bool,
        Option<f64>,
    );
    // The drain is read with the health (one statement): it lives on the
    // deployment and its connection, the circuit on the health row.
    let sql = format!(
        "SELECT d.id,h.consecutive_failures,EXTRACT(EPOCH FROM h.open_until-clock_timestamp())::float8,EXTRACT(EPOCH FROM clock_timestamp()-h.last_observed_at)::float8,EXTRACT(EPOCH FROM h.probe_until-clock_timestamp())::float8,h.reopen_count,{},{} FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id LEFT JOIN deployment_health h ON h.deployment_id=d.id WHERE d.id=ANY($1)",
        drain::ROUTE_DRAINED,
        drain::DRAIN_ENDS_IN
    );
    let rows: Vec<HealthRow> = sqlx::query_as(&sql)
        .bind(ids)
        .fetch_all(&store.pool)
        .await
        .map_err(route_lookup_storage)?;
    let mut fresh: Vec<(Uuid, HealthEntry)> = ids
        .iter()
        .map(|id| {
            (
                *id,
                HealthEntry {
                    loaded,
                    open_until: None,
                    consecutive_failures: 0,
                    observed_age: None,
                    probe_until: None,
                    reopen_count: 0,
                    drained: false,
                    drain_until: None,
                },
            )
        })
        .collect();
    for (id, failures, open_for, observed, probe_for, reopens, drained, drain_for) in rows {
        if let Some(entry) = fresh.iter_mut().find(|(d, _)| *d == id) {
            entry.1.drained = drained;
            entry.1.drain_until = drain_for
                .filter(|_| drained)
                .map(|s| loaded + std::time::Duration::from_secs_f64(s.clamp(0.0, 1e9)));
            let (Some(failures), Some(reopens)) = (failures, reopens) else {
                // No health row: a route that never failed.
                continue;
            };
            entry.1.consecutive_failures = failures;
            // An expired circuit is kept, as of the load (half-open).
            entry.1.open_until = open_for
                .map(|s| loaded + std::time::Duration::from_secs_f64(s.clamp(0.0, 86_400.0)));
            entry.1.probe_until = probe_for
                .filter(|s| *s > 0.0)
                .map(|s| loaded + std::time::Duration::from_secs_f64(s.min(86_400.0)));
            entry.1.reopen_count = reopens;
            entry.1.observed_age =
                observed.map(|s| std::time::Duration::from_secs_f64(s.clamp(0.0, 1e9)));
        }
    }
    Ok(fresh)
}

/// A route's circuit as it is now (through the caches, at most
/// `HEALTH_TTL` old across replicas; this replica's own writes are seen at
/// once): what a request about to send on a route planned earlier must
/// respect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteState {
    /// An operator drain applies (`routing::drain`): no new attempt, whatever
    /// the circuit; for about this many more seconds, when it ends within the
    /// hour (`drain::retry_after`).
    Drained(Option<u32>),
    Closed,
    /// The cooldown has passed: the route is used only as its probe.
    HalfOpen,
    /// Open for about this many more seconds.
    Open(u32),
}

pub async fn route_state(store: &Store, deployment: Uuid) -> Result<RouteState, InferenceError> {
    let entry = match store.caches.health_entry(deployment) {
        Some(entry) => entry,
        None => {
            let fresh = load_health(store, &[deployment]).await?;
            let entry = fresh[0].1;
            if store.caches.enabled() {
                store.caches.store_health(fresh);
            }
            entry
        }
    };
    let now = std::time::Instant::now();
    if entry.drained_at(now) {
        return Ok(RouteState::Drained(drain::retry_after(
            entry.drain_until.map(|until| (until - now).as_secs_f64()),
        )));
    }
    Ok(match entry.open_until {
        None => RouteState::Closed,
        Some(until) if until > now => {
            RouteState::Open((until - now).as_secs_f64().ceil().clamp(1.0, 86_400.0) as u32)
        }
        Some(_) => RouteState::HalfOpen,
    })
}

fn validate_ids(ids: &[Uuid]) -> Result<(), InferenceError> {
    if ids.len() > MAX_CANDIDATES || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(InferenceError::Configuration);
    }
    if ids.is_empty() {
        return Err(InferenceError::ModelUnavailable);
    }
    Ok(())
}

fn eligible_candidates(
    policy: &RoutingPolicy,
    mut candidates: Vec<Candidate>,
    probing: bool,
) -> Result<Vec<Candidate>, InferenceError> {
    policy.validate()?;
    validate_ids(
        &candidates
            .iter()
            .map(|c| c.deployment_id)
            .collect::<Vec<_>>(),
    )?;
    // Validate even disabled candidates: invalid operator configuration must not be hidden.
    if candidates
        .iter()
        .any(|c| !(1..=1000).contains(&c.weight) || !valid_residency(&c.residency))
    {
        return Err(InferenceError::Configuration);
    }
    candidates.retain(|c| {
        !c.operator_disabled
            && policy
                .required_residency
                .as_deref()
                .is_none_or(|label| label == c.residency)
    });
    // A drained route takes no new attempt (and is never a probe). If drains
    // are the only reason nothing can serve the model, it is temporarily
    // unavailable, with `Retry-After` only for a drain that ends soon.
    let drained = candidates.iter().any(|c| c.drained);
    let drain_retry = candidates
        .iter()
        .filter(|c| c.drained)
        .filter_map(|c| drain::retry_after(c.drain_ends_in))
        .min();
    candidates.retain(|c| !c.drained);
    // Routes in cooldown are skipped, and for a probing caller a half-open
    // route whose probe another request holds. If that is the only reason
    // nothing can serve the model, say so (retryable, with the soonest
    // reopening) instead of reporting the model as missing.
    let busy = |c: &Candidate| probing && c.half_open && c.probe_held_seconds > 0;
    let cooling = candidates
        .iter()
        .filter_map(|c| {
            if c.circuit_open {
                Some(c.cooldown_remaining_seconds)
            } else if busy(c) {
                Some(c.probe_held_seconds.min(PROBE_RETRY_AFTER_SECONDS))
            } else {
                None
            }
        })
        .map(|seconds| seconds.max(1) as u32)
        .min();
    candidates.retain(|c| !c.circuit_open && !busy(c));
    if !probing {
        // This caller decides no probe: a half-open route is an expired cooldown.
        for c in &mut candidates {
            c.half_open = false;
        }
    }
    if candidates.is_empty()
        && let Some(seconds) = cooling
    {
        return Err(InferenceError::RouteCoolingDown(
            drain_retry.map_or(seconds, |drain| drain.min(seconds)),
        ));
    }
    if candidates.is_empty() && drained {
        return Err(InferenceError::RouteDrained(drain_retry));
    }
    // Stable sorting retains repository order when priority values tie.
    if candidates.is_empty() {
        return Err(InferenceError::ModelUnavailable);
    }
    candidates.sort_by_key(|c| c.priority);
    Ok(candidates)
}

fn weighted_index(candidates: &[Candidate], rng: &mut StdRng) -> Result<usize, InferenceError> {
    let tier = candidates
        .iter()
        .take_while(|c| c.priority == candidates[0].priority)
        .count();
    let weights = WeightedIndex::new(candidates[..tier].iter().map(|c| c.weight as u32))
        .map_err(|_| InferenceError::Configuration)?;
    Ok(weights.sample(rng))
}

fn order_candidates(
    policy: &RoutingPolicy,
    candidates: Vec<Candidate>,
    seed: [u8; 32],
    probing: bool,
) -> Result<RoutePlan, InferenceError> {
    let mut candidates = eligible_candidates(policy, candidates, probing)?;
    let mut ordered = Vec::with_capacity(candidates.len());
    if policy.strategy == "weighted" {
        let mut rng = StdRng::from_seed(seed);
        while !candidates.is_empty() {
            ordered.push(candidates.remove(weighted_index(&candidates, &mut rng)?));
        }
    } else {
        ordered = candidates;
    }
    let first = ordered.first().ok_or(InferenceError::ModelUnavailable)?;
    // Residency is an explicit safety boundary, never inferred from a region/provider.
    let residency = first.residency.clone();
    let first_id = first.deployment_id;
    if policy.max_attempts > 1 {
        ordered.retain(|c| {
            c.deployment_id == first_id || (residency != "unspecified" && c.residency == residency)
        });
    }
    Ok(RoutePlan {
        probes: ordered
            .iter()
            .filter(|c| c.half_open)
            .map(|c| c.deployment_id)
            .collect(),
        deployment_ids: ordered.into_iter().map(|c| c.deployment_id).collect(),
        max_attempts: policy.max_attempts as usize,
        allow_ambiguous_failover: policy.allow_ambiguous_failover,
        held_probe: None,
    })
}

fn valid_residency(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// This decision is valid ONLY before any ProviderOutput::Stream was returned,
/// even if that stream has not been polled. The engine also enforces the attempt
/// budget and one shared hard deadline; no inference retry occurs in this module.
pub fn may_failover(plan: &RoutePlan, error: InferenceError) -> bool {
    plan.max_attempts > 1
        && match error {
            InferenceError::Busy => true,
            // Ambiguous transport failures may already have incurred provider charges.
            InferenceError::UpstreamUnavailable | InferenceError::Overloaded => {
                plan.allow_ambiguous_failover
            }
            _ => false,
        }
}

/// [`may_failover`] for a failure the engine proved happened before the
/// attempt built any upstream request (`inference::dispatch`): nothing reached
/// the provider, so a route's own unavailability (an unresolvable credential
/// or endpoint is `Configuration`) is not ambiguous and may fail over under
/// the same explicit policy as `Busy`, without `allow_ambiguous_failover`.
/// Client errors and the shared deadline (`Timeout`) still do not: another
/// route would see the same request and the same deadline. Valid only for a
/// failure returned before any stream, like [`may_failover`].
pub fn may_fail_over_unsent(plan: &RoutePlan, error: InferenceError) -> bool {
    plan.max_attempts > 1
        && matches!(
            error,
            InferenceError::Busy
                | InferenceError::Configuration
                | InferenceError::UpstreamUnavailable
                | InferenceError::Overloaded
        )
}

mod health;
pub(crate) use health::HEALTH_VIEW_SQL;
#[cfg(test)]
use health::affects_health;
pub use health::{
    HEALTH_STALE_AFTER_SECONDS, RouteHealth, health, record_probe_result, record_result,
    record_unclaimed_result,
};
pub(crate) mod drain;
mod probe;
pub use probe::{
    PROBE_RETRY_AFTER_SECONDS, ProbeClaim, claim_probe, close_probe, release_probe, renew_probe,
};
#[cfg(all(test, feature = "integration-tests"))]
mod drain_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod probe_tests;
#[cfg(test)]
mod tests;
