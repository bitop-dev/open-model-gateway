//! Which route a video request goes to, and why none does: the caller's
//! routes of the model whose video options (0097) take its length and
//! size, and the explicit refusals naming what none of them takes.
use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use super::*;

/// `POST /v1/videos` for a model none of whose routes has video options.
const NO_VIDEO_ROUTE: &str = "No route of this model is set up for video generation";
const SECONDS_NOT_TAKEN: &str = "No route of this model accepts this clip length";
const SIZE_NOT_TAKEN: &str = "No route of this model accepts this size";
const COMBINATION_NOT_TAKEN: &str =
    "No route of this model accepts this clip length with this size";

/// Why none of the caller's routes of the model (enabled, allowed to its
/// workspace and key, serving video) took a video request: none is set up
/// for video, none takes its length, none takes its size, or each is taken
/// by some route but no route takes both (`param` then names neither, as
/// neither alone is wrong). When some route
/// takes both, the request was refused for another reason (its routes do not
/// serve it): `Unsupported`, which the caller answers with the routing
/// outcome.
fn refusal(routes: &HashMap<Uuid, VideoRoute>, request: &VideoRequest) -> JobError {
    if routes.is_empty() {
        return JobError::Conflict("unsupported_capability", NO_VIDEO_ROUTE);
    }
    if routes.values().any(|r| r.accepts(request).is_ok()) {
        return InferenceError::Unsupported.into();
    }
    let takes: Vec<_> = routes.values().map(|r| r.takes(request)).collect();
    if !takes.iter().any(|(seconds, _)| *seconds) {
        JobError::InvalidParameter(
            "unsupported_video_parameter",
            SECONDS_NOT_TAKEN,
            VideoParameter::Seconds.field(),
        )
    } else if !takes.iter().any(|(_, size)| *size) {
        JobError::InvalidParameter(
            "unsupported_video_parameter",
            SIZE_NOT_TAKEN,
            VideoParameter::Size.field(),
        )
    } else {
        JobError::Conflict("unsupported_video_parameter", COMBINATION_NOT_TAKEN)
    }
}

fn price_missing() -> JobError {
    JobError::Conflict("video_price_missing", super::video::VIDEO_PRICE_MISSING)
}

impl Jobs {
    /// The route a video request goes to, among the caller's routes of the
    /// model whose options (0097) take its length and size, and the request
    /// as that route sends it (its length resolved); otherwise the refusal
    /// naming what none of the caller's routes takes ([`Self::refusal_of`]).
    pub(super) async fn video_route(
        &self,
        principal: &Principal,
        request: &VideoRequest,
        request_id: Uuid,
    ) -> JobResult<(Deployment, Arc<dyn ProviderAdapter>, VideoRequest)> {
        let candidates = self.video_candidates(principal, request, false).await?;
        let routes = self.video_routes(&candidates).await?;
        let priced = self.priced_routes(&routes, request).await?;
        let selected = self
            .select(
                principal,
                &request.model,
                ApiProtocol::Videos,
                request_id,
                |a, d| priced.contains(&d.id) && a.supports_video_request(d, request),
            )
            .await;
        let (target, adapter) = match selected {
            Err(e @ (InferenceError::Unsupported | InferenceError::ModelUnavailable)) => {
                return Err(self.refusal_of(principal, request, request_id, e).await);
            }
            other => other?,
        };
        let resolved = routes
            .get(&target.id)
            .and_then(|r| r.accepts(request).ok())
            .ok_or(InferenceError::Configuration)?;
        Ok((target, adapter, resolved))
    }

    /// Admit the video under the catalog lock. A route changed between its
    /// choice and the locked admission is answered as it would have been
    /// before admission, classified on the request as the caller sent it
    /// (`original`: a length it did not name is never blamed).
    pub(super) async fn admit_video(
        &self,
        principal: Principal,
        request_id: Uuid,
        original: &VideoRequest,
        target: &Deployment,
        admission: &WorkloadAdmission,
        check: &crate::governance::VideoCheck,
    ) -> JobResult<Attempt> {
        match self
            .admit(
                principal,
                request_id,
                &original.model,
                target,
                admission,
                Some(check),
            )
            .await
        {
            Ok(attempt) => Ok(attempt),
            // The route's price stopped valuing the request after its choice
            // (`governance::video::admit_check`); any other unbounded price
            // (a budget over an unbounded meter) is answered as for every
            // workload.
            Err(InferenceError::PriceUnbounded) => {
                match crate::governance::video_route_priced(
                    &self.store,
                    target.id,
                    check.size.as_deref(),
                )
                .await
                {
                    Ok(false) => Err(price_missing()),
                    Ok(true) => Err(InferenceError::PriceUnbounded.into()),
                    Err(e) => Err(e.into()),
                }
            }
            Err(InferenceError::Unsupported) => Err(self
                .refusal_of(
                    &principal,
                    original,
                    request_id,
                    InferenceError::Unsupported,
                )
                .await),
            Err(e) => Err(e.into()),
        }
    }

    /// The caller's video routes of the model (enabled, offered to its
    /// workspace and allowed to its key, on an adapter that serves video);
    /// `live`: read now, not from the per-replica candidate cache.
    async fn video_candidates(
        &self,
        principal: &Principal,
        request: &VideoRequest,
        live: bool,
    ) -> JobResult<Vec<Deployment>> {
        let deployments = if live {
            self.store
                .live_deployments(principal, &request.model)
                .await?
        } else {
            self.store.deployments(principal, &request.model).await?
        };
        Ok(deployments
            .into_iter()
            .filter(|d| {
                d.supported_protocols
                    .iter()
                    .any(|p| p == ApiProtocol::Videos.as_str())
                    && self
                        .registry
                        .get(&d.provider)
                        .is_some_and(|a| a.supports_protocol(ApiProtocol::Videos))
            })
            .collect())
    }

    /// Why no route took a video request, classified only over the routes
    /// a request of this caller can be sent to now: its routes read live
    /// (never a cached list that may outlive its access) and then filtered as
    /// routing filters them (residency, operator disablement). A model it may
    /// not use, or none of whose routes routing would take, answers as
    /// routing does; otherwise the refusal names what none of them takes,
    /// or, when one would take the request, the routing outcome stands.
    async fn refusal_of(
        &self,
        principal: &Principal,
        request: &VideoRequest,
        request_id: Uuid,
        outcome: InferenceError,
    ) -> JobError {
        // A name that can never match is an unknown model (it would break
        // the live query).
        if !crate::inference::types::model_name_is_queryable(&request.model) {
            return InferenceError::ModelUnavailable.into();
        }
        let candidates = match self.video_candidates(principal, request, true).await {
            Ok(candidates) if candidates.is_empty() => {
                return InferenceError::ModelUnavailable.into();
            }
            Ok(candidates) => candidates,
            Err(e) => return e,
        };
        // Each route planned on its own: routing's filters (residency,
        // operator disablement) decide whether it could take a request,
        // never another route's place in a plan.
        let mut eligible = Vec::new();
        let mut planned = None;
        for candidate in candidates {
            match self
                .store
                .route_plan(
                    principal,
                    &request.model,
                    std::slice::from_ref(&candidate),
                    request_id,
                )
                .await
            {
                Ok(_) => eligible.push(candidate.id),
                Err(e) => planned = planned.or(Some(e)),
            }
        }
        if eligible.is_empty() {
            return planned.unwrap_or(InferenceError::ModelUnavailable).into();
        }
        let routes = match self.video_routes_by_id(&eligible).await {
            Ok(routes) => routes,
            Err(e) => return e,
        };
        match refusal(&routes, request) {
            // A route takes the request; none of them prices it.
            JobError::Inference(InferenceError::Unsupported) => {
                match self.priced_routes(&routes, request).await {
                    Ok(priced) if priced.is_empty() => price_missing(),
                    Ok(_) => outcome.into(),
                    Err(e) => e,
                }
            }
            refused => refused,
        }
    }

    /// The routes that take the request and whose latest price values it
    /// as settlement will ([`crate::governance::video_route_priced`]): only
    /// these are chosen, so a priced route is never passed over for an
    /// unpriced one (admission checks the price again under its lock).
    async fn priced_routes(
        &self,
        routes: &HashMap<Uuid, VideoRoute>,
        request: &VideoRequest,
    ) -> JobResult<HashSet<Uuid>> {
        let mut priced = HashSet::new();
        for (id, route) in routes {
            let Ok(resolved) = route.accepts(request) else {
                continue;
            };
            if crate::governance::video_route_priced(&self.store, *id, resolved.size.as_deref())
                .await?
            {
                priced.insert(*id);
            }
        }
        Ok(priced)
    }

    /// The video options (0097) of these routes; a route without them is not
    /// listed.
    async fn video_routes(
        &self,
        candidates: &[Deployment],
    ) -> JobResult<HashMap<Uuid, VideoRoute>> {
        let ids: Vec<Uuid> = candidates.iter().map(|d| d.id).collect();
        self.video_routes_by_id(&ids).await
    }
    async fn video_routes_by_id(&self, ids: &[Uuid]) -> JobResult<HashMap<Uuid, VideoRoute>> {
        let rows: Vec<(Uuid, Vec<i32>, Option<Vec<String>>)> = sqlx::query_as(
            "SELECT id,video_seconds,video_sizes FROM deployments WHERE id=ANY($1) AND video_seconds IS NOT NULL",
        )
        .bind(ids)
        .fetch_all(&self.store.pool)
        .await
        .map_err(|_| InferenceError::Storage)?;
        Ok(rows
            .into_iter()
            .map(|(id, seconds, sizes)| {
                let seconds = seconds
                    .into_iter()
                    .filter_map(|s| u32::try_from(s).ok())
                    .collect();
                (id, VideoRoute { seconds, sizes })
            })
            .collect())
    }
}
