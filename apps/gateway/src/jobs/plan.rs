//! Batch validation and planning at create time. The input file is streamed
//! once: every line is checked (shape, unique `custom_id`, the endpoint's body
//! contract, model available to this key, priced and bounded), valid lines
//! are copied to the batch's private file (internal, never listed; the client may
//! delete its input later), and the lines are grouped by deployment for the
//! batch's ceiling. Invalid files are rejected with a line-numbered report.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::{
    io::{LazySink, LineReader},
    lines::{LineError, parse_line},
    *,
};
use crate::{
    filestore::StoredFile,
    governance::batch::{LineGroup, LinePreview, LinePricing},
    routing::RouteInputs,
};

mod scan;

/// At most this many problems are reported.
pub const MAX_ISSUES: usize = 20;

/// One reported problem: the 1-based physical line number of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Issue {
    pub line: u64,
    pub error: LineError,
}

pub(crate) enum PlanError {
    Invalid(Vec<Issue>),
    Job(JobError),
}
impl From<JobError> for PlanError {
    fn from(e: JobError) -> Self {
        Self::Job(e)
    }
}
impl From<InferenceError> for PlanError {
    fn from(e: InferenceError) -> Self {
        Self::Job(e.into())
    }
}

/// The validated batch.
pub(crate) struct Plan {
    pub groups: Vec<LineGroup>,
    pub requests: u32,
    /// Set when the batch runs natively (one deployment, adapter support).
    pub native: Option<(Deployment, Arc<dyn ProviderAdapter>)>,
    pub work_file: Uuid,
    /// The model label (one model, or `mixed`).
    pub model: String,
}

struct Resolved {
    deployment: Deployment,
    adapter: Arc<dyn ProviderAdapter>,
    /// A model set up as "Batch" (0016): native Chat Completions only.
    legacy: bool,
}

/// Test-only counters for candidate, routing-input and price reads in a scan.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PlanLookupCounts {
    pub(crate) deployments: std::sync::atomic::AtomicUsize,
    pub(crate) route_plan: std::sync::atomic::AtomicUsize,
    pub(crate) prices: std::sync::atomic::AtomicUsize,
}

/// Reuse the inputs for per-line routing within one scan, even when the
/// adapter verdict and seed differ on every line. Dispatch checks its own
/// live inputs.
#[derive(Default)]
struct RoutingMemo {
    deployments: HashMap<String, Arc<Vec<Deployment>>>,
    routes: HashMap<String, RouteInputs>,
    /// Route sets for refusal classification, which uses a batch-wide seed.
    /// Adapter verdicts still belong to the individual line.
    usable: HashMap<(String, Vec<Uuid>), Arc<Vec<Uuid>>>,
}

#[derive(Default)]
struct LineRouting {
    estimate: Option<Resolved>,
    gateway_error: Option<InferenceError>,
    rejection: Option<InferenceError>,
}

/// What [`Jobs::scan`] returns: the groups, the request count, the native
/// route (when the batch runs natively) and the model label.
type Scanned = (
    Vec<LineGroup>,
    u32,
    Option<(Deployment, Arc<dyn ProviderAdapter>)>,
    String,
);

/// The per-scan arguments every line step needs.
#[derive(Clone, Copy)]
struct ScanScope<'a> {
    principal: &'a Principal,
    request_id: Uuid,
    endpoint: BatchEndpoint,
    force_gateway: bool,
}

/// A deployment's group, built while scanning.
struct PendingGroup {
    group: LineGroup,
    /// The first line number using the deployment.
    first_line: u64,
    adapter: Arc<dyn ProviderAdapter>,
    legacy: bool,
}

/// Route and price diagnostics for a mode that might not be used.
#[derive(Default)]
struct PreviewErrors {
    issues: Vec<Issue>,
    job: Option<InferenceError>,
}

/// The scan's mutable state.
struct ScanState {
    issues: Vec<Issue>,
    ids: HashSet<String>,
    models: HashMap<String, Result<Arc<Resolved>, LineError>>,
    // Cache TTL requirements are part of the memoization key: a clean
    // line must not authorize a later cached line at an incompatible price.
    previews: HashMap<(Uuid, Option<u32>, u8), LinePreview>,
    // A failed load is shared by all preview keys for that deployment.
    prices: HashMap<Uuid, Result<LinePricing, InferenceError>>,
    // One per deployment, in first-use order.
    native_groups: Vec<PendingGroup>,
    // Gateway estimates can split a model across differently priced routes.
    // Native selection and pricing still use the batch-wide deployment.
    gateway_groups: Vec<PendingGroup>,
    native_preview_errors: PreviewErrors,
    gateway_preview_errors: PreviewErrors,
    requests: u32,
    physical: u64,
    // Native eligibility is tracked independently of the pricing previews.
    native_ok: bool,
    native_deployment: Option<Uuid>,
    gateway_unsupported: Option<u64>,
    // Lines the coarse check passed but the adapter's own pure encoder
    // (`ProviderAdapter::validate_chat_request`, #228) refuses
    // deterministically: excluded from the parent envelope, not from the
    // batch (see the end of the scan).
    deterministic_rejects: Vec<(u64, InferenceError)>,
    routing: RoutingMemo,
}

impl Jobs {
    async fn resolve(
        &self,
        principal: &Principal,
        model: &str,
        endpoint: BatchEndpoint,
        request_id: Uuid,
    ) -> Result<Result<Resolved, LineError>, JobError> {
        let picked = self
            .select(principal, model, endpoint.protocol(), request_id, |_, _| {
                true
            })
            .await;
        let picked = match picked {
            Err(InferenceError::Unsupported) if endpoint == BatchEndpoint::ChatCompletions => self
                .select(
                    principal,
                    model,
                    ApiProtocol::Batches,
                    request_id,
                    |a, d| a.native_batch(d, endpoint),
                )
                .await
                .map(|(d, a)| (d, a, true)),
            other => other.map(|(d, a)| (d, a, false)),
        };
        Ok(match picked {
            Ok((deployment, adapter, legacy)) => Ok(Resolved {
                deployment,
                adapter,
                legacy,
            }),
            Err(InferenceError::ModelUnavailable) => Err(LineError::ModelNotFound),
            Err(InferenceError::Unsupported) => Err(LineError::ModelUnsupported),
            Err(e) => return Err(e.into()),
        })
    }

    /// Estimate the scheduler's first compatible route for an unpinned line
    /// or classify a deterministic refusal when every capable route rejects
    /// the body. Candidate filtering precedes route selection here and at
    /// dispatch. This preview does not admit a line.
    async fn line_routing(
        &self,
        scope: ScanScope<'_>,
        model: &str,
        request: &BatchRequest,
        line_index: u32,
        model_route: Uuid,
        memo: &mut RoutingMemo,
    ) -> Result<LineRouting, JobError> {
        let protocol = scope.endpoint.protocol();
        let deployments = match memo.deployments.get(model) {
            Some(cached) => cached.clone(),
            None => {
                #[cfg(test)]
                self.plan_lookup_counts
                    .deployments
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let fetched = Arc::new(self.store.deployments(scope.principal, model).await?);
                memo.deployments.insert(model.to_owned(), fetched.clone());
                fetched
            }
        };
        let (candidates, refused) = self.line_candidates(protocol, request, &deployments);
        if refused.is_empty() && (candidates.is_empty() || deployments.len() == 1) {
            return Ok(LineRouting::default());
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = memo.routes.entry(model.to_owned())
        {
            #[cfg(test)]
            self.plan_lookup_counts
                .route_plan
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            slot.insert(
                RouteInputs::load(&self.store, scope.principal, model, &deployments).await?,
            );
        }
        let routes = &memo.routes[model];
        if !candidates.is_empty() {
            let id = match routes.first_route(
                scope.principal,
                model,
                lines::routing_seed(scope.request_id, line_index),
                &candidates,
            ) {
                Ok(id) => id,
                // A compatible model route within policy may wait out
                // cooldown. The scheduler will check it again at dispatch.
                Err(InferenceError::RouteCoolingDown(_))
                    if candidates.contains(&model_route) && routes.is_cooling(model_route) =>
                {
                    return Ok(LineRouting::default());
                }
                Err(
                    error
                    @ (InferenceError::RouteCoolingDown(_) | InferenceError::ModelUnavailable),
                ) => {
                    return Ok(LineRouting {
                        gateway_error: Some(error),
                        ..LineRouting::default()
                    });
                }
                Err(e) => return Err(e.into()),
            };
            let deployment = deployments
                .iter()
                .find(|d| d.id == id)
                .ok_or(InferenceError::Configuration)?
                .clone();
            let adapter = self
                .registry
                .get(&deployment.provider)
                .ok_or(InferenceError::Configuration)?;
            return Ok(LineRouting {
                estimate: Some(Resolved {
                    deployment,
                    adapter,
                    legacy: false,
                }),
                ..LineRouting::default()
            });
        }
        // Every capable adapter refused. Classify from the routes the
        // routing policy would use, using this line's own adapter verdicts.
        let mut key_ids: Vec<Uuid> = refused.iter().map(|(id, _)| *id).collect();
        key_ids.sort_unstable();
        let key = (model.to_owned(), key_ids);
        let usable = match memo.usable.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let ids = refused.iter().map(|(id, _)| *id).collect();
                let plan = routes.plan_for(scope.principal, model, scope.request_id, &ids)?;
                let ids = Arc::new(plan.deployment_ids);
                memo.usable.insert(key, ids.clone());
                ids
            }
        };
        let invalid = refused.iter().find(|(id, error)| {
            matches!(
                error,
                InferenceError::InvalidRequest | InferenceError::InvalidField(_)
            ) && usable.contains(id)
        });
        Ok(LineRouting {
            rejection: Some(invalid.map_or(InferenceError::Unsupported, |(_, e)| *e)),
            ..LineRouting::default()
        })
    }

    /// Per-body protocol capability and adapter validation match dispatch.
    fn line_candidates(
        &self,
        protocol: ApiProtocol,
        request: &BatchRequest,
        deployments: &[Deployment],
    ) -> (HashSet<Uuid>, Vec<(Uuid, InferenceError)>) {
        let mut candidates = HashSet::new();
        let mut refused = Vec::new();
        let mut verdicts: HashMap<String, Result<(), InferenceError>> = HashMap::new();
        for deployment in deployments {
            if !deployment
                .supported_protocols
                .iter()
                .any(|p| p == protocol.as_str())
            {
                continue;
            }
            let Some(adapter) = self.registry.get(&deployment.provider) else {
                continue;
            };
            if !adapter.supports_protocol(protocol) {
                continue;
            }
            let verdict = match request {
                BatchRequest::Chat(r) if adapter.supports_chat_request(r) => {
                    *verdicts.entry(deployment.provider.clone()).or_insert_with(|| {
                        match adapter.validate_chat_request(r) {
                            verdict @ (Ok(()) | Err(InferenceError::InvalidRequest | InferenceError::InvalidField(_) | InferenceError::Unsupported)) => verdict,
                            Err(other) => {
                                tracing::warn!(provider = %deployment.provider, error = ?other,
                                    "adapter request validation returned an error outside its contract");
                                Err(InferenceError::Unsupported)
                            }
                        }
                    })
                }
                BatchRequest::Embeddings(r) if adapter.supports_embedding_target(deployment, r) => Ok(()),
                _ => continue,
            };
            match verdict {
                Ok(()) => {
                    candidates.insert(deployment.id);
                }
                Err(error) => refused.push((deployment.id, error)),
            }
        }
        (candidates, refused)
    }

    /// Validate `input` (already opened) and copy it to the batch's private
    /// file. See the module docs.
    pub(crate) async fn plan(
        &self,
        principal: &Principal,
        request_id: Uuid,
        input: &StoredFile,
        endpoint: BatchEndpoint,
        force_gateway: bool,
    ) -> Result<Plan, PlanError> {
        let files = self.files()?;
        let mut reader = LineReader::stored(
            files,
            input.id,
            principal.workspace_id,
            BATCH_MAX_LINE_BYTES,
        )
        .await
        .map_err(|_| JobError::NotFound)?;
        let mut work = LazySink::new(
            files,
            io::batch_file(
                true,
                principal.workspace_id,
                principal.key_id,
                principal.user_id,
                format!("batch_{}_input.jsonl", request_id.simple()),
            ),
        );
        let result = self
            .scan(
                principal,
                request_id,
                endpoint,
                force_gateway,
                &mut reader,
                &mut work,
            )
            .await;
        match result {
            Ok((groups, requests, native, model)) => {
                let work_file = work.finish().await?.ok_or(PlanError::Invalid(vec![Issue {
                    line: 1,
                    error: LineError::EmptyFile,
                }]))?;
                Ok(Plan {
                    groups,
                    requests,
                    native,
                    work_file,
                    model,
                })
            }
            Err(e) => {
                work.abort().await;
                Err(e)
            }
        }
    }

    async fn scan(
        &self,
        principal: &Principal,
        request_id: Uuid,
        endpoint: BatchEndpoint,
        force_gateway: bool,
        reader: &mut LineReader,
        work: &mut LazySink,
    ) -> Result<Scanned, PlanError> {
        let scope = ScanScope {
            principal,
            request_id,
            endpoint,
            force_gateway,
        };
        let mut state = ScanState::new();
        loop {
            let raw = match reader.next().await {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(InferenceError::InvalidUpstream) => {
                    state.issue(state.physical + 1, LineError::LineTooLarge);
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            state.physical += 1;
            if lines::is_blank(&raw) {
                continue;
            }
            if state.requests >= BATCH_MAX_REQUESTS {
                state.issue(state.physical, LineError::TooManyLines);
                break;
            }
            state.requests += 1;
            let line = match self.check_line(scope, &mut state, &raw).await? {
                Ok(line) => line,
                Err(e) => {
                    if state.issue(state.physical, e) {
                        break;
                    }
                    continue;
                }
            };
            if !state.issues.is_empty() {
                // Keep scanning for problems, but stop copying.
                continue;
            }
            state.record(endpoint, &line);
            scan::copy_line(work, raw, &line).await?;
        }
        state.finish()
    }
}

impl ScanState {
    fn new() -> Self {
        Self {
            issues: Vec::new(),
            ids: HashSet::new(),
            models: HashMap::new(),
            previews: HashMap::new(),
            prices: HashMap::new(),
            native_groups: Vec::new(),
            gateway_groups: Vec::new(),
            native_preview_errors: PreviewErrors::default(),
            gateway_preview_errors: PreviewErrors::default(),
            requests: 0,
            physical: 0,
            native_ok: true,
            native_deployment: None,
            gateway_unsupported: None,
            deterministic_rejects: Vec::new(),
            routing: RoutingMemo::default(),
        }
    }

    /// Report a problem; `true` once [`MAX_ISSUES`] are reported.
    fn issue(&mut self, line: u64, error: LineError) -> bool {
        self.issues.push(Issue { line, error });
        self.issues.len() >= MAX_ISSUES
    }

    /// The whole-file checks after the last line, then the result.
    fn finish(mut self) -> Result<Scanned, PlanError> {
        let native = self.native_ok && self.native_deployment.is_some();
        let (groups, preview_errors) = if native {
            (
                std::mem::take(&mut self.native_groups),
                std::mem::take(&mut self.native_preview_errors),
            )
        } else {
            if self.issues.is_empty() {
                self.gateway_issue();
            }
            (
                std::mem::take(&mut self.gateway_groups),
                std::mem::take(&mut self.gateway_preview_errors),
            )
        };
        self.issues.extend(preview_errors.issues);
        self.issues.sort_by_key(|issue| issue.line);
        self.issues.truncate(MAX_ISSUES);
        if !self.issues.is_empty() {
            return Err(PlanError::Invalid(self.issues));
        }
        if let Some(error) = preview_errors.job {
            return Err(error.into());
        }
        if groups.is_empty() {
            self.report_rejects();
            return Err(PlanError::Invalid(self.issues));
        }
        let first_model = &groups[0].group.model;
        let model = if groups.iter().all(|g| g.group.model == *first_model) {
            first_model.clone()
        } else {
            "mixed".to_owned()
        };
        let native = native.then(|| {
            (
                groups[0].group.deployment.clone(),
                groups[0].adapter.clone(),
            )
        });
        Ok((
            groups.into_iter().map(|g| g.group).collect(),
            self.requests,
            native,
            model,
        ))
    }

    /// A batch that runs through the gateway cannot hold lines the engine
    /// cannot run.
    fn gateway_issue(&mut self) {
        // Lines only a native batch could run.
        if let Some(g) = self.gateway_groups.iter().find(|g| g.legacy) {
            self.issues.push(Issue {
                line: g.first_line,
                error: LineError::ModelUnsupported,
            });
        } else if let Some(line) = self.gateway_unsupported {
            self.issues.push(Issue {
                line,
                error: LineError::UnsupportedFeature,
            });
        }
    }

    fn report_rejects(&mut self) {
        // Every candidate line was a deterministic adapter rejection:
        // there is nothing to reserve or dispatch. Refuse the upload
        // with a line-numbered report, like any other invalid line,
        // instead of admitting an empty batch.
        for (line, error) in std::mem::take(&mut self.deterministic_rejects) {
            let error = match error {
                InferenceError::InvalidRequest | InferenceError::InvalidField(_) => {
                    LineError::InvalidBody
                }
                InferenceError::Unsupported => LineError::UnsupportedFeature,
                other => {
                    tracing::warn!(error = ?other, "adapter request validation returned an error outside its contract");
                    LineError::UnsupportedFeature
                }
            };
            if self.issue(line, error) {
                break;
            }
        }
        if self.issues.is_empty() {
            self.issues.push(Issue {
                line: 1,
                error: LineError::EmptyFile,
            });
        }
    }
}
