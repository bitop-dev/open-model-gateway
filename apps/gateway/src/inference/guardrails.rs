//! The engine's guardrail step (docs/guardrails-design.md §3 "Input"): after
//! candidate filtering and before the route plan, once per root request, so a
//! refused request reserves nothing and reaches no provider. Failover reuses
//! the inspected request; nothing is inspected twice.
//!
//! The workspace's compiled policy comes from a per-replica cache under the
//! `guardrails` and `access` topics. A decision is recorded in
//! `guardrail_events` (ids, codes and counts) and counted in
//! `gateway_guardrail_decisions_total`; the text is dropped as soon as the
//! decision is made.
use std::sync::Arc;

use uuid::Uuid;

use super::error::InferenceError;
use super::repository::route_lookup_storage;
use crate::auth::Principal;
use crate::cache::Lookup;
use crate::guardrails::detector::Inspection;
use crate::guardrails::enforce::{Event, Policy};
use crate::guardrails::policy::{Workload, compose};
use crate::guardrails::stored;
use crate::store::Store;

/// What the engine records for one request's decision.
pub struct GuardrailRecord<'a> {
    pub root_request_id: Uuid,
    pub principal: Principal,
    /// The client operation (`guardrail_events.operation`).
    pub operation: &'static str,
    /// The catalog model the request named.
    pub model: &'a str,
    /// Whether the client asked for a stream.
    pub streamed: bool,
    pub events: &'a [Event],
}

impl Store {
    /// A workspace's compiled guardrail policy (cached per replica while the
    /// `guardrails` and `access` versions are unchanged; empty policies too).
    pub(crate) async fn guardrail_policy(
        &self,
        workspace: Uuid,
    ) -> Result<Arc<Policy>, InferenceError> {
        let stamp = match self
            .caches
            .guardrails
            .get(&self.caches.versions, &workspace)
        {
            Lookup::Hit(policy) => return Ok(policy),
            Lookup::Miss(stamp) => Some(stamp),
            Lookup::Bypass => None,
        };
        let mut conn = self.pool.acquire().await.map_err(route_lookup_storage)?;
        let (platform, local) = stored::workspace_layers(&mut conn, workspace)
            .await
            .map_err(route_lookup_storage)?
            // A stored value this release cannot read refuses the request.
            .map_err(|_| InferenceError::Storage)?;
        // Compiling custom patterns takes milliseconds each: off the async workers.
        let effective = compose(platform.as_ref(), local.as_ref());
        let policy = Arc::new(
            tokio::task::spawn_blocking(move || Policy::compile(&effective))
                .await
                .map_err(|_| InferenceError::Storage)?,
        );
        if let Some(stamp) = stamp {
            self.caches
                .guardrails
                .insert(workspace, Arc::clone(&policy), stamp);
        }
        Ok(policy)
    }

    /// The guardrail step of a request whose text this release does not read
    /// (a native batch: its lines are encoded for the provider's own batch
    /// API): an enforcing rule refuses it, a monitor rule records `skipped`.
    pub(crate) async fn guard_unread(
        &self,
        principal: &Principal,
        request: Guarded<'_>,
    ) -> Result<(), InferenceError> {
        let policy = self.guardrail_policy(principal.workspace_id).await?;
        let decision = policy.decide(request.workload, None);
        count(&decision);
        if !decision.events.is_empty() {
            self.record_guardrail_events(&GuardrailRecord {
                root_request_id: request.request_id,
                principal: *principal,
                operation: request.operation,
                model: request.model,
                streamed: request.streamed,
                events: &decision.events,
            })
            .await?;
        }
        decision
            .refusal
            .map_or(Ok(()), |r| Err(InferenceError::Guardrail(r)))
    }

    /// Append one request's decisions (one statement, outside any admission
    /// transaction: a refused request takes no lock and holds nothing).
    pub(crate) async fn record_guardrail_events(
        &self,
        record: &GuardrailRecord<'_>,
    ) -> Result<(), InferenceError> {
        let e = record.events;
        let col = |f: fn(&Event) -> String| e.iter().map(f).collect::<Vec<_>>();
        sqlx::query(
            "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,streamed,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused)
             SELECT id,$2,$3,$4,$5,$6,$16,'input',rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused
             FROM unnest($1::uuid[],$7::uuid[],$8::uuid[],$9::bigint[],$10::text[],$11::text[],$12::text[],$13::text[],$14::int[],$15::boolean[])
              AS t(id,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused)",
        )
        .bind(e.iter().map(|_| Uuid::now_v7()).collect::<Vec<_>>())
        .bind(record.root_request_id)
        .bind(record.principal.workspace_id)
        .bind(record.principal.key_id)
        .bind(record.operation)
        .bind(record.model)
        .bind(e.iter().map(|v| v.rule_set_id).collect::<Vec<_>>())
        .bind(e.iter().map(|v| v.rule_id).collect::<Vec<_>>())
        .bind(e.iter().map(|v| v.revision).collect::<Vec<_>>())
        .bind(col(|v| v.layer.as_str().to_owned()))
        .bind(col(|v| v.detector.as_str().to_owned()))
        .bind(col(|v| v.category.as_str().to_owned()))
        .bind(col(|v| v.action.as_str().to_owned()))
        .bind(
            e.iter()
                .map(|v| i32::try_from(v.count).unwrap_or(i32::MAX))
                .collect::<Vec<_>>(),
        )
        .bind(e.iter().map(|v| v.refused).collect::<Vec<_>>())
        .bind(record.streamed)
        .execute(&self.pool)
        .await
        .map_err(route_lookup_storage)?;
        Ok(())
    }
}

/// The facts of a request the guardrail step records.
pub(crate) struct Guarded<'a> {
    pub(crate) request_id: Uuid,
    pub(crate) operation: &'static str,
    pub(crate) model: &'a str,
    pub(crate) streamed: bool,
    pub(crate) workload: Workload,
}

/// Count a decision's events in the metrics.
fn count(decision: &crate::guardrails::enforce::Decision) {
    for event in &decision.events {
        crate::metrics::METRICS.guardrail_decision(
            "input",
            event.detector.as_str(),
            event.category.as_str(),
            event.action.as_str(),
        );
    }
}

impl super::Engine {
    /// The guardrail step of a generation request, within its deadline.
    pub(super) async fn guard_chat(
        &self,
        principal: &Principal,
        request: &super::types::ChatRequest,
        request_id: Uuid,
        protocol: super::types::ApiProtocol,
        deadline: tokio::time::Instant,
    ) -> Result<(), InferenceError> {
        let facts = Guarded {
            request_id,
            operation: operation(protocol),
            model: &request.model,
            streamed: request.stream,
            workload: Workload::Generation,
        };
        tokio::time::timeout_at(
            deadline,
            self.guard_input(principal, facts, || {
                Some(crate::guardrails::enforce::chat_inspection(request))
            }),
        )
        .await
        .map_err(|_| InferenceError::Timeout)?
    }
}

/// `guardrail_events.operation` of a generation protocol.
pub(super) fn operation(protocol: super::types::ApiProtocol) -> &'static str {
    use super::types::ApiProtocol;
    match protocol {
        ApiProtocol::Responses => "responses",
        ApiProtocol::Messages => "messages",
        ApiProtocol::Embeddings => "embeddings",
        _ => "chat_completions",
    }
}

impl super::Engine {
    /// Inspect a request's input under its workspace's policy. `Ok` lets it
    /// continue; a refusal is `InferenceError::Guardrail`. The text is
    /// collected only when a rule applies to `workload`, and dropped when the
    /// decision is made.
    pub(super) async fn guard_input<'a>(
        &self,
        principal: &Principal,
        request: Guarded<'_>,
        input: impl FnOnce() -> Option<Inspection<'a>>,
    ) -> Result<(), InferenceError> {
        let Guarded {
            request_id,
            operation,
            model,
            streamed,
            workload,
        } = request;
        let policy = self.repository.guardrails(principal.workspace_id).await?;
        if !policy.applies_to(workload) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let decision = {
            let input = input();
            policy.decide(workload, input.as_ref())
        };
        crate::metrics::METRICS.guardrail_detector_seconds("pattern", started.elapsed());
        count(&decision);
        if !decision.events.is_empty() {
            self.repository
                .record_guardrails(&GuardrailRecord {
                    root_request_id: request_id,
                    principal: *principal,
                    operation,
                    model,
                    streamed,
                    events: &decision.events,
                })
                .await?;
        }
        match decision.refusal {
            Some(refusal) => Err(InferenceError::Guardrail(refusal)),
            None => Ok(()),
        }
    }
}
