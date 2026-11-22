//! Per-line batch checks: parse, model, price, engine support and the
//! adapter's deterministic refusal; then sizing and copying an accepted line.
use super::*;

/// A line with its available estimates for each execution mode.
pub(super) struct CheckedLine {
    model: String,
    pin: Option<lines::LinePlan>,
    native: Option<PricedRoute>,
    gateway: Option<PricedRoute>,
    deterministic_reject: Option<InferenceError>,
}

struct PricedRoute {
    resolved: Arc<Resolved>,
    price: LinePrice,
}

#[derive(Clone, Copy)]
struct LinePrice {
    id: Uuid,
    output: u32,
}

struct LinePrices {
    primary: Option<LinePrice>,
    gateway: Option<PricedRoute>,
}

type PricingCheck<T> = Result<Result<T, LineError>, InferenceError>;

impl PreviewErrors {
    fn defer(&mut self, line: u64, check: PricingCheck<LinePrice>) -> Option<LinePrice> {
        match check {
            Ok(Ok(price)) => return Some(price),
            Ok(Err(error)) if self.issues.len() < MAX_ISSUES => {
                self.issues.push(Issue { line, error });
            }
            Ok(Err(_)) => {}
            Err(error) => {
                self.job.get_or_insert(error);
            }
        }
        None
    }
}

impl Jobs {
    /// Check shape, `custom_id`, model, routing, price bounds and adapter
    /// support. Keep distinct pricing outcomes while auto mode is unresolved.
    pub(super) async fn check_line(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        raw: &[u8],
    ) -> Result<Result<CheckedLine, LineError>, PlanError> {
        let line = match parse_line(raw, scope.endpoint) {
            Ok(l) => l,
            Err(e) => return Ok(Err(e)),
        };
        if !state.ids.insert(line.custom_id.clone()) {
            return Ok(Err(LineError::DuplicateCustomId));
        }
        let model = line.request.model().to_owned();
        let mut resolved = match self.resolve_once(scope, &mut state.models, &model).await? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        let requested = match &line.request {
            BatchRequest::Chat(r) => r.max_output_tokens,
            BatchRequest::Embeddings(_) => Some(0),
        };
        let LineRouting {
            estimate,
            gateway_error,
            rejection,
        } = if requested.is_some() && !resolved.legacy {
            self.line_routing(
                scope,
                &model,
                &line.request,
                state.requests - 1,
                resolved.deployment.id,
                &mut state.routing,
            )
            .await?
        } else {
            LineRouting::default()
        };
        // Native eligibility depends on encoding and a single deployment.
        // Track that deployment even when its pricing preview fails.
        state.native_ok &= !scope.force_gateway
            && state
                .native_deployment
                .is_none_or(|id| id == resolved.deployment.id)
            && resolved
                .adapter
                .native_batch(&resolved.deployment, scope.endpoint);
        if requested.is_some() && !resolved.legacy {
            state.check_native_encoding(scope.endpoint, &line.request, &resolved);
            state.native_ok &= rejection.is_none();
        }
        let has_estimate = estimate.is_some();
        let alternate = if state.native_ok {
            estimate.filter(|route| route.deployment.id != resolved.deployment.id)
        } else {
            if let Some(estimate) = estimate {
                resolved = Arc::new(estimate);
            }
            None
        };
        let LinePrices { primary, gateway } = match self
            .price_routes(
                scope,
                state,
                &line.request,
                &resolved,
                requested,
                gateway_error.map_or(Ok(alternate), Err),
            )
            .await?
        {
            Ok(prices) => prices,
            Err(error) => return Ok(Err(error)),
        };
        let pin = if let (None, Some(price)) = (requested, primary) {
            Some(lines::LinePlan {
                deployment: resolved.deployment.id,
                price: price.id,
                output: price.output,
            })
        } else {
            None
        };
        let rejection =
            match state.check_engine(scope.endpoint, line.request, &resolved, pin, has_estimate) {
                Ok(pinned_rejection) => rejection.or(pinned_rejection),
                Err(error) => return Ok(Err(error)),
            };
        state.native_ok &= rejection.is_none();
        let native = if state.native_ok {
            primary.map(|price| PricedRoute { resolved, price })
        } else {
            None
        };
        Ok(Ok(CheckedLine {
            model,
            pin,
            native,
            gateway,
            deterministic_reject: rejection,
        }))
    }

    async fn price_routes(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        request: &BatchRequest,
        resolved: &Arc<Resolved>,
        requested: Option<u32>,
        alternate: Result<Option<Resolved>, InferenceError>,
    ) -> PricingCheck<LinePrices> {
        let alternate = match alternate {
            Ok(route) => route,
            Err(error) => {
                // A native encoding has its own batch-wide route and price.
                // Only gateway mode requires a compatible per-line route.
                state.gateway_preview_errors.job.get_or_insert(error);
                let primary = if state.native_ok {
                    let check = self
                        .price_line(scope, state, request, resolved, requested)
                        .await;
                    state.native_preview_errors.defer(state.physical, check)
                } else {
                    None
                };
                return Ok(Ok(LinePrices {
                    primary,
                    gateway: None,
                }));
            }
        };
        let price = self
            .price_line(scope, state, request, resolved, requested)
            .await;
        let Some(alternate) = alternate else {
            return Ok(price?.map(|price| LinePrices {
                primary: Some(price),
                gateway: Some(PricedRoute {
                    resolved: resolved.clone(),
                    price,
                }),
            }));
        };
        // An explicit maximum can be checked against both routes. Only
        // the preview for the final mode can reject the file.
        let alternate_price = self
            .price_line(scope, state, request, &alternate, requested)
            .await;
        Ok(Ok(LinePrices {
            primary: state.native_preview_errors.defer(state.physical, price),
            gateway: state
                .gateway_preview_errors
                .defer(state.physical, alternate_price)
                .map(|price| PricedRoute {
                    resolved: Arc::new(alternate),
                    price,
                }),
        }))
    }

    /// The model's deployment for this key, resolved at most once per scan.
    async fn resolve_once(
        &self,
        scope: ScanScope<'_>,
        models: &mut HashMap<String, Result<Arc<Resolved>, LineError>>,
        model: &str,
    ) -> Result<Result<Arc<Resolved>, LineError>, PlanError> {
        if let std::collections::hash_map::Entry::Vacant(slot) = models.entry(model.to_owned()) {
            slot.insert(
                self.resolve(scope.principal, model, scope.endpoint, scope.request_id)
                    .await?
                    .map(Arc::new),
            );
        }
        Ok(models[model].clone())
    }

    /// The line's price and output maximum, previewed at most once per
    /// deployment, requested maximum and cache TTL mask.
    async fn price_line(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        request: &BatchRequest,
        resolved: &Resolved,
        requested: Option<u32>,
    ) -> PricingCheck<LinePrice> {
        let cache_controls = match request {
            BatchRequest::Chat(r) if resolved.deployment.provider == "anthropic" => r
                .messages_options
                .as_ref()
                .map(|o| o.cache_controls.as_slice())
                .unwrap_or_default(),
            _ => &[],
        };
        let cache_mask = cache_controls
            .iter()
            .fold(0_u8, |mask, c| mask | if c["ttl"] == "1h" { 2 } else { 1 });
        let key = (resolved.deployment.id, requested, cache_mask);
        if let std::collections::hash_map::Entry::Vacant(slot) = state.previews.entry(key) {
            let id = resolved.deployment.id;
            if let std::collections::hash_map::Entry::Vacant(price) = state.prices.entry(id) {
                #[cfg(test)]
                self.plan_lookup_counts
                    .prices
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                price.insert(LinePricing::load(&self.store, id).await);
            }
            slot.insert(
                state.prices[&id]
                    .as_ref()
                    .map_err(|error| *error)?
                    .preview(
                        scope.endpoint.protocol(),
                        requested,
                        resolved.adapter.generation_output_limit(),
                        cache_controls,
                    )?,
            );
        }
        Ok(match state.previews[&key] {
            LinePreview::Ok { price_id, output } => Ok(LinePrice {
                id: price_id,
                output,
            }),
            LinePreview::OutputLimit => Err(LineError::OutputLimit),
            LinePreview::Unpriced | LinePreview::Unbounded => Err(LineError::Unpriced),
        })
    }
}

/// Copy an accepted line to the work file.
pub(super) async fn copy_line(
    work: &mut LazySink,
    raw: Vec<u8>,
    line: &CheckedLine,
) -> Result<(), PlanError> {
    let mut copy = match line.pin {
        Some(pin) => lines::planned_copy(raw, pin)?,
        None => raw,
    };
    copy.push(b'\n');
    work.raw(copy).await?;
    Ok(())
}

impl ScanState {
    /// Native encoding is checked on the batch-wide route while it is
    /// eligible. Legacy lines always require native encoding.
    fn check_native_encoding(
        &mut self,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        resolved: &Resolved,
    ) -> bool {
        if !self.native_ok && !resolved.legacy {
            return false;
        }
        let encodable = resolved
            .adapter
            .encode_native_line(&resolved.deployment, endpoint, "l0", request)
            .is_ok();
        self.native_ok &= encodable;
        if self.native_ok {
            self.native_deployment = Some(resolved.deployment.id);
        }
        encodable
    }

    /// Gateway support and the encoding checks that require a pinned bound.
    fn check_engine(
        &mut self,
        endpoint: BatchEndpoint,
        mut request: BatchRequest,
        resolved: &Resolved,
        pin: Option<lines::LinePlan>,
        has_estimate: bool,
    ) -> Result<Option<InferenceError>, LineError> {
        let mut rejection = None;
        let coarse_ok = resolved.legacy
            || match &request {
                BatchRequest::Chat(r) => resolved.adapter.supports_chat_request(r),
                BatchRequest::Embeddings(r) => resolved
                    .adapter
                    .supports_embedding_target(&resolved.deployment, r),
            };
        if resolved.legacy || pin.is_some() {
            // Omitted maxima must use the plan's clamped body for encoding
            // and deterministic adapter validation.
            if let (Some(pin), BatchRequest::Chat(r)) = (pin, &mut request) {
                r.max_output_tokens = Some(pin.output);
                if !resolved.legacy && coarse_ok {
                    rejection = resolved.adapter.validate_chat_request(r).err();
                }
            }
            let encodable = self.check_native_encoding(endpoint, &request, resolved);
            if resolved.legacy && !encodable {
                return Err(LineError::UnsupportedFeature);
            }
        }
        if !coarse_ok && !has_estimate && self.gateway_unsupported.is_none() {
            self.gateway_unsupported = Some(self.physical);
        }
        Ok(rejection)
    }

    /// Size an accepted line into its deployment's group, or set it aside
    /// as a deterministic rejection.
    pub(super) fn record(&mut self, endpoint: BatchEndpoint, line: &CheckedLine) {
        if let Some(error) = line.deterministic_reject {
            // A deterministic refusal holds no budget. Still dispatch it
            // so the engine can report the line's ordinary refusal.
            self.deterministic_rejects.push((self.physical, error));
            return;
        }
        if let Some(route) = &line.native {
            record_group(
                &mut self.native_groups,
                self.physical,
                endpoint,
                line,
                route,
            );
        }
        if let Some(route) = &line.gateway {
            record_group(
                &mut self.gateway_groups,
                self.physical,
                endpoint,
                line,
                route,
            );
        }
    }
}

fn record_group(
    groups: &mut Vec<PendingGroup>,
    physical: u64,
    endpoint: BatchEndpoint,
    line: &CheckedLine,
    route: &PricedRoute,
) {
    let resolved = &route.resolved;
    let output = route.price.output;
    let price_id = line.pin.map(|pin| pin.price);
    match groups
        .iter_mut()
        .find(|g| g.group.deployment.id == resolved.deployment.id)
    {
        Some(g) => {
            *g.group.outputs.entry(output).or_insert(0) += 1;
            if price_id.is_some() {
                g.group.price_id = price_id;
            }
        }
        None => groups.push(PendingGroup {
            group: LineGroup {
                deployment: resolved.deployment.clone(),
                model: line.model.clone(),
                protocol: endpoint.protocol(),
                outputs: BTreeMap::from([(output, 1)]),
                price_id,
            },
            first_line: physical,
            adapter: resolved.adapter.clone(),
            legacy: resolved.legacy,
        }),
    }
}
