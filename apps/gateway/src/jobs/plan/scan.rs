//! Per-line batch checks: parse, model, price, engine support and the
//! adapter's deterministic refusal; then sizing and copying an accepted line.
use super::*;

/// A line that passed every per-line check.
pub(super) struct CheckedLine {
    model: String,
    resolved: Arc<Resolved>,
    /// The line's own output maximum (`Some(0)` for embeddings).
    requested: Option<u32>,
    price_id: Uuid,
    max: u32,
    deterministic_reject: Option<InferenceError>,
    gateway_estimate: Option<(Resolved, u32)>,
}

impl Jobs {
    /// Every per-line check, in order: shape, unique `custom_id`, model,
    /// price and bound, native/engine support and the adapter's own
    /// deterministic refusal.
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
        let mut routing = if requested.is_some() && !resolved.legacy {
            self.line_routing(
                scope,
                &model,
                &line.request,
                state.requests - 1,
                &mut state.routing,
            )
            .await?
        } else {
            LineRouting::default()
        };
        // Once native mode is ruled out, validate the weighted estimate
        // directly. When auto mode may still be native, keep its separate
        // batch-wide estimate until the whole input has been checked.
        state.native_ok &= !scope.force_gateway
            && resolved
                .adapter
                .native_batch(&resolved.deployment, scope.endpoint);
        let has_estimate = routing.estimate.is_some();
        if !state.native_ok
            && let Some(estimate) = routing.estimate.take()
        {
            resolved = Arc::new(estimate);
        }
        let (price_id, max) = match self
            .price_line(scope, state, &line.request, &resolved, requested)
            .await?
        {
            Ok(priced) => priced,
            Err(e) => return Ok(Err(e)),
        };
        // Encoding checks use the same bound as the parent hold; the
        // uploaded body and private body retain the optional omission.
        let mut execution_request = line.request.clone();
        if let BatchRequest::Chat(r) = &mut execution_request {
            r.max_output_tokens = Some(max);
        }
        let coarse_ok =
            match state.engine_checks(scope.endpoint, &line.request, &execution_request, &resolved)
            {
                Ok(coarse_ok) => coarse_ok,
                Err(e) => return Ok(Err(e)),
            };
        if requested.is_none()
            && !resolved.legacy
            && coarse_ok
            && let BatchRequest::Chat(r) = &execution_request
        {
            routing.rejection = resolved.adapter.validate_chat_request(r).err();
        }
        if !coarse_ok && !has_estimate && state.gateway_unsupported.is_none() {
            state.gateway_unsupported = Some(state.physical);
        }
        let gateway_estimate = self
            .gateway_estimate(
                scope,
                state,
                &line.request,
                &resolved,
                requested,
                routing.estimate,
            )
            .await;
        Ok(Ok(CheckedLine {
            model,
            resolved,
            requested,
            price_id,
            max,
            deterministic_reject: routing.rejection,
            gateway_estimate,
        }))
    }

    /// A native batch sizes against its one deployment. Keep an alternate
    /// price error until the scan knows whether it will run in gateway mode.
    async fn gateway_estimate(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        request: &BatchRequest,
        resolved: &Resolved,
        requested: Option<u32>,
        estimate: Option<Resolved>,
    ) -> Option<(Resolved, u32)> {
        let estimate = estimate.filter(|d| d.deployment.id != resolved.deployment.id)?;
        let error = match self
            .price_line(scope, state, request, &estimate, requested)
            .await
        {
            Ok(Ok((_, output))) => return Some((estimate, output)),
            Ok(Err(error)) => PlanError::Invalid(vec![Issue {
                line: state.physical,
                error,
            }]),
            Err(error) => error,
        };
        state.gateway_pricing_error.get_or_insert(error);
        None
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
    ) -> Result<Result<(Uuid, u32), LineError>, PlanError> {
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
                price.insert(LinePricing::load(&self.store, id).await?);
            }
            slot.insert(state.prices[&id].preview(
                scope.endpoint.protocol(),
                requested,
                resolved.adapter.generation_output_limit(),
                cache_controls,
            )?);
        }
        Ok(match state.previews[&key] {
            LinePreview::Ok { price_id, output } => Ok((price_id, output)),
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
    let mut copy = if line.requested.is_none() {
        lines::planned_copy(
            raw,
            lines::LinePlan {
                deployment: line.resolved.deployment.id,
                price: line.price_id,
                output: line.max,
            },
        )?
    } else {
        raw
    };
    copy.push(b'\n');
    work.raw(copy).await?;
    Ok(())
}

impl ScanState {
    /// Whether the engine can run the line (the coarse check). Native
    /// encodability is folded into `native_ok`; a legacy line must be
    /// encodable.
    fn engine_checks(
        &mut self,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution_request: &BatchRequest,
        resolved: &Resolved,
    ) -> Result<bool, LineError> {
        // Can this line run natively, and can the engine run it?
        let encodable = resolved
            .adapter
            .native_batch(&resolved.deployment, endpoint)
            && resolved
                .adapter
                .encode_native_line(&resolved.deployment, endpoint, "l0", execution_request)
                .is_ok();
        if resolved.legacy && !encodable {
            return Err(LineError::UnsupportedFeature);
        }
        self.native_ok &= encodable;
        Ok(resolved.legacy
            || match request {
                BatchRequest::Chat(r) => resolved.adapter.supports_chat_request(r),
                BatchRequest::Embeddings(r) => resolved
                    .adapter
                    .supports_embedding_target(&resolved.deployment, r),
            })
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
        record_group(
            &mut self.groups,
            self.physical,
            endpoint,
            line,
            &line.resolved,
            line.max,
        );
        let (resolved, output) = line
            .gateway_estimate
            .as_ref()
            .map_or((line.resolved.as_ref(), line.max), |(route, output)| {
                (route, *output)
            });
        record_group(
            &mut self.gateway_groups,
            self.physical,
            endpoint,
            line,
            resolved,
            output,
        );
    }
}

fn record_group(
    groups: &mut Vec<PendingGroup>,
    physical: u64,
    endpoint: BatchEndpoint,
    line: &CheckedLine,
    resolved: &Resolved,
    output: u32,
) {
    let price_id = line.requested.is_none().then_some(line.price_id);
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
