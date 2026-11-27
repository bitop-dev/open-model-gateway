//! Which route a video request goes to, and why none does: the caller's
//! routes of the model whose video options (0097) take its length and
//! size, and the explicit refusals naming what none of them takes.
use std::collections::HashMap;

use uuid::Uuid;

use super::*;

/// `POST /v1/videos` for a model none of whose routes has video options.
const NO_VIDEO_ROUTE: &str = "No route of this model is set up for video generation";
const SECONDS_NOT_TAKEN: &str = "No route of this model accepts this clip length";
const SIZE_NOT_TAKEN: &str = "No route of this model accepts this size";

/// Why none of the caller's routes of the model (enabled, allowed to its
/// workspace and key, serving video) took a video request: none is set up
/// for video, none takes its length, or none takes its size. When some route
/// takes both, the request was refused for another reason (its routes do not
/// serve it): `Unsupported`, which the caller answers with the routing
/// outcome.
fn refusal(routes: &HashMap<Uuid, VideoRoute>, request: &VideoRequest) -> JobError {
    if routes.is_empty() {
        return JobError::Conflict("unsupported_capability", NO_VIDEO_ROUTE);
    }
    let answers: Vec<_> = routes.values().map(|r| r.accepts(request)).collect();
    if answers.iter().any(Result::is_ok) {
        return InferenceError::Unsupported.into();
    }
    if answers
        .iter()
        .all(|a| matches!(a, Err(VideoParameter::Seconds)))
    {
        JobError::InvalidParameter(
            "unsupported_video_parameter",
            SECONDS_NOT_TAKEN,
            VideoParameter::Seconds.field(),
        )
    } else {
        JobError::InvalidParameter(
            "unsupported_video_parameter",
            SIZE_NOT_TAKEN,
            VideoParameter::Size.field(),
        )
    }
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
        let selected = self
            .select(
                principal,
                &request.model,
                ApiProtocol::Videos,
                request_id,
                |a, d| {
                    routes
                        .get(&d.id)
                        .is_some_and(|r| r.accepts(request).is_ok())
                        && a.supports_video_request(d, request)
                },
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
    /// before admission.
    pub(super) async fn admit_video(
        &self,
        principal: Principal,
        request_id: Uuid,
        request: &VideoRequest,
        target: &Deployment,
        admission: &WorkloadAdmission,
        check: &crate::governance::VideoCheck,
    ) -> JobResult<Attempt> {
        match self
            .admit(
                principal,
                request_id,
                &request.model,
                target,
                admission,
                Some(check),
            )
            .await
        {
            Ok(attempt) => Ok(attempt),
            Err(InferenceError::PriceUnbounded) => Err(JobError::Conflict(
                "video_price_missing",
                super::video::VIDEO_PRICE_MISSING,
            )),
            Err(InferenceError::Unsupported) => Err(self
                .refusal_of(&principal, request, request_id, InferenceError::Unsupported)
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
        let candidates = match self.video_candidates(principal, request, true).await {
            Ok(candidates) if candidates.is_empty() => {
                return InferenceError::ModelUnavailable.into();
            }
            Ok(candidates) => candidates,
            Err(e) => return e,
        };
        let plan = match self
            .store
            .route_plan(principal, &request.model, &candidates, request_id)
            .await
        {
            Ok(plan) => plan,
            Err(e) => return e.into(),
        };
        match self.video_routes_by_id(&plan.deployment_ids).await {
            Ok(routes) => match refusal(&routes, request) {
                JobError::Inference(InferenceError::Unsupported) => outcome.into(),
                refused => refused,
            },
            Err(e) => e,
        }
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
