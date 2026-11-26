//! The engine's guardrail step (docs/guardrails-design.md §3 "Input", §4):
//! after candidate filtering and before the route plan, once per root
//! request, so a refused request reserves nothing and reaches no provider.
//! Failover reuses the inspected request; nothing is inspected twice.
//!
//! The workspace's compiled policy comes from a per-replica cache under the
//! `guardrails` and `access` topics; a stale entry whose rule sets are
//! unchanged is revalidated with one read, never recompiled. A decision is
//! recorded in `guardrail_events` (ids, codes and counts; at most
//! `MAX_EVENT_ROWS` rows per request) and counted in
//! `gateway_guardrail_decisions_total`; the text is dropped as soon as the
//! decision is made. Scans that leave the async workers take bounded,
//! per-workspace scan slots (`guardrails::limits`).
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use uuid::Uuid;

use super::error::InferenceError;
use super::repository::route_lookup_storage;
use crate::auth::Principal;
use crate::cache::Lookup;
use crate::guardrails::detector::Inspection;
use crate::guardrails::enforce::{Decision, Event, Policy, RefusalKind};
use crate::guardrails::limits::{
    self, LOCAL_SCANS, MAX_EVENT_ROWS, RECORDED, REFUSALS, SHARED_SCANS,
};
use crate::guardrails::patterns::{LOCAL_COMPILES, PLATFORM_COMPILES};
use crate::guardrails::policy::{Workload, compose};
use crate::guardrails::stored::{self, Heads};
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
    /// The rows to write (the refused one first).
    pub events: &'a [Event],
    /// Further decisions of the request that were not written as rows.
    pub omitted: u32,
}

/// A cached policy and the rule-set heads it was compiled from.
pub struct CachedPolicy {
    heads: Heads,
    policy: Arc<Policy>,
}

/// One compile per workspace at a time on this replica: concurrent misses
/// wait for it instead of compiling copies.
static LOADS: LazyLock<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// Holds a workspace's load lock; the map entry goes with its last holder or
/// waiter (an entry a dropped waiter leaves behind goes with the workspace's
/// next load).
struct Load {
    workspace: Uuid,
    lock: Arc<tokio::sync::Mutex<()>>,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}
impl Load {
    async fn enter(workspace: Uuid) -> Self {
        let lock = {
            let mut loads = LOADS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(loads.entry(workspace).or_default())
        };
        let mut load = Self {
            workspace,
            lock,
            held: None,
        };
        load.held = Some(Arc::clone(&load.lock).lock_owned().await);
        load
    }
}
impl Drop for Load {
    fn drop(&mut self) {
        self.held = None;
        let mut loads = LOADS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only the map and this holder: nobody else waits for it.
        if Arc::strong_count(&self.lock) == 2 {
            loads.remove(&self.workspace);
        }
    }
}

/// Inputs above this many bytes, and any input a custom pattern scans (its
/// cost may be any user's), are scanned off the async workers in scan slots.
const LARGE_INPUT: usize = 64 * 1024;

impl Store {
    /// A workspace's compiled guardrail policy (empty ones too): a cache hit
    /// costs no statement; a stale or unconfirmed entry costs one read of the
    /// rule-set heads, and a recompile only when they changed. A confirmed
    /// miss waits for a load of the workspace already running and is then
    /// served by it; with unconfirmed versions every lookup reads the heads
    /// (in parallel) and only compiles wait for each other.
    pub(crate) async fn guardrail_policy(
        &self,
        workspace: Uuid,
    ) -> Result<Arc<Policy>, InferenceError> {
        let cache = &self.caches.guardrails;
        let versions = &self.caches.versions;
        // Peeked before each lookup, which drops a stale entry.
        let mut stale = cache.peek(&workspace);
        let mut load = None;
        let stamp = match cache.get(versions, &workspace) {
            Lookup::Hit(entry) => return Ok(Arc::clone(&entry.policy)),
            Lookup::Bypass => None,
            Lookup::Miss(_) => {
                load = Some(Load::enter(workspace).await);
                // One stored while this request waited is newer.
                stale = cache.peek(&workspace).or(stale);
                match cache.get(versions, &workspace) {
                    // Loaded by the request this one waited for.
                    Lookup::Hit(entry) => return Ok(Arc::clone(&entry.policy)),
                    Lookup::Miss(stamp) => Some(stamp),
                    Lookup::Bypass => None,
                }
            }
        };
        let keep = |entry: Arc<CachedPolicy>| {
            let policy = Arc::clone(&entry.policy);
            match stamp {
                Some(stamp) => cache.insert(workspace, entry, stamp),
                None => cache.insert_unconfirmed(workspace, entry),
            }
            policy
        };
        let read_heads = async || {
            let mut conn = self.pool.acquire().await.map_err(route_lookup_storage)?;
            let heads = stored::workspace_heads(&mut conn, workspace)
                .await
                .map_err(route_lookup_storage)?;
            Ok::<_, InferenceError>((conn, heads))
        };
        let (mut conn, mut heads) = read_heads().await?;
        if let Some(entry) = stale.filter(|e| e.heads == heads) {
            // Unconfirmed, it is served as it is: storing it again could
            // replace a newer compile.
            return Ok(if stamp.is_some() {
                keep(entry)
            } else {
                Arc::clone(&entry.policy)
            });
        }
        if load.is_none() {
            // Unconfirmed: the heads were read without the lock, so read them
            // again under it; compiles then run one at a time, each from the
            // heads current when it started, and the last one stored is the
            // latest.
            drop(conn);
            let _ = load.insert(Load::enter(workspace).await);
            (conn, heads) = read_heads().await?;
            if let Some(entry) = cache.peek(&workspace).filter(|e| e.heads == heads) {
                return Ok(Arc::clone(&entry.policy));
            }
        }
        let Ok((platform, local)) = stored::layers_of(&mut conn, heads.clone())
            .await
            .map_err(route_lookup_storage)?
        else {
            tracing::warn!(workspace_id = %workspace, "a stored guardrail rule is not readable by this release");
            return Err(InferenceError::GuardrailUnavailable);
        };
        drop(conn);
        let effective = compose(platform.as_ref(), local.as_ref());
        let patterns = |set: &Option<crate::guardrails::policy::RuleSet>| {
            set.iter().flat_map(|s| &s.rules).any(|r| {
                matches!(&r.entry, crate::guardrails::policy::Entry::Rule(rule) if !rule.detector.patterns.is_empty())
            })
        };
        // Compiling custom patterns takes milliseconds each: in a compile
        // slot, off the async workers and without a connection, waiting for
        // one at most as long as for a scan slot; a policy without them
        // compiles at once.
        let slots = if patterns(&local) {
            Some(&LOCAL_COMPILES)
        } else if patterns(&platform) {
            Some(&PLATFORM_COMPILES)
        } else {
            None
        };
        let policy = match slots {
            Some(slots) => {
                crate::guardrails::patterns::in_slot_within(slots, Some(limits::WAIT), move || {
                    Policy::compile(&effective)
                })
                .await
            }
            None => Some(Policy::compile(&effective)),
        }
        .ok_or(InferenceError::GuardrailUnavailable)?;
        Ok(keep(Arc::new(CachedPolicy {
            heads,
            policy: Arc::new(policy),
        })))
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
        if !policy.applies_to(request.workload) {
            return Ok(());
        }
        throttled(principal)?;
        let decision = policy.decide(request.workload, None);
        let (events, omitted) = rows(&decision);
        if !events.is_empty() {
            self.record_guardrail_events(&GuardrailRecord {
                root_request_id: request.request_id,
                principal: *principal,
                operation: request.operation,
                model: request.model,
                streamed: request.streamed,
                events: &events,
                omitted,
            })
            .await?;
            RECORDED.note(principal.key_id, limits::now());
        }
        verdict(principal, &decision)
    }

    /// Append one request's decisions (one statement, outside any admission
    /// transaction: a refused request takes no lock and holds nothing).
    pub(crate) async fn record_guardrail_events(
        &self,
        record: &GuardrailRecord<'_>,
    ) -> Result<(), InferenceError> {
        let e = record.events;
        let col = |f: fn(&Event) -> String| e.iter().map(f).collect::<Vec<_>>();
        let omitted: Vec<i32> = (0..e.len())
            .map(|i| {
                if i == 0 {
                    i32::try_from(record.omitted).unwrap_or(i32::MAX)
                } else {
                    0
                }
            })
            .collect();
        sqlx::query(
            "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,streamed,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused,tightened,omitted)
             SELECT id,$2,$3,$4,$5,$6,$16,'input',rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused,tightened,omitted
             FROM unnest($1::uuid[],$7::uuid[],$8::uuid[],$9::bigint[],$10::text[],$11::text[],$12::text[],$13::text[],$14::int[],$15::boolean[],$17::boolean[],$18::int[])
              AS t(id,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count,refused,tightened,omitted)",
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
        .bind(e.iter().map(|v| v.tightened).collect::<Vec<_>>())
        .bind(omitted)
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
fn count(decision: &Decision) {
    for event in &decision.events {
        crate::metrics::METRICS.guardrail_decision(
            "input",
            event.detector.as_str(),
            event.category.as_str(),
            event.action.as_str(),
        );
    }
}

/// The rows to write: the refused decision first, then the others, at most
/// `MAX_EVENT_ROWS`; the number left out is recorded on the first row.
fn rows(decision: &Decision) -> (Vec<Event>, u32) {
    count(decision);
    let mut events = decision.events.clone();
    events.sort_by_key(|e| !e.refused);
    let omitted = events.len().saturating_sub(MAX_EVENT_ROWS);
    events.truncate(MAX_EVENT_ROWS);
    (events, u32::try_from(omitted).unwrap_or(u32::MAX))
}

/// 429 before any scan when the key's refusals reached this minute's bound.
fn throttled(principal: &Principal) -> Result<(), InferenceError> {
    let now = limits::now();
    REFUSALS
        .check(principal.key_id, now)
        .and(RECORDED.check(principal.key_id, now))
        .map_err(InferenceError::GuardrailThrottled)
}

/// The request's outcome; a refusal counts toward the key's bound.
fn verdict(principal: &Principal, decision: &Decision) -> Result<(), InferenceError> {
    match decision.refusal {
        Some(refusal) => {
            REFUSALS.note(principal.key_id, limits::now());
            Err(InferenceError::Guardrail(refusal))
        }
        None => Ok(()),
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
    ) -> Result<Unadmitted, InferenceError> {
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

    /// Inspect a request's input under its workspace's policy. `Ok` lets it
    /// continue, with the [`Unadmitted`] mark the caller clears once an
    /// attempt is admitted; a refusal is an `InferenceError::Guardrail*`. The
    /// text is collected only when a rule applies to `workload`, and dropped
    /// when the decision is made.
    pub(super) async fn guard_input<'a>(
        &self,
        principal: &Principal,
        request: Guarded<'_>,
        input: impl FnOnce() -> Option<Inspection<'a>>,
    ) -> Result<Unadmitted, InferenceError> {
        let Guarded {
            request_id,
            operation,
            model,
            streamed,
            workload,
        } = request;
        let policy = self.repository.guardrails(principal.workspace_id).await?;
        if !policy.applies_to(workload) {
            return Ok(Unadmitted::new(None));
        }
        throttled(principal)?;
        let input = input();
        let decision = match input {
            // Off the async workers, in a bounded slot that moves into the
            // scan (held until the scan ends even when the request is
            // dropped). No slot in time: every rule is unavailable.
            Some(input) if input.len() > LARGE_INPUT || policy.has_custom() => {
                let slots = if policy.has_local_custom() {
                    &LOCAL_SCANS
                } else {
                    &SHARED_SCANS
                };
                match slots.acquire(principal.workspace_id).await {
                    Ok(slot) => {
                        let owned = input.into_owned();
                        let policy = Arc::clone(&policy);
                        tokio::task::spawn_blocking(move || {
                            let _slot = slot;
                            timed(|| policy.decide(workload, Some(&owned)))
                        })
                        .await
                        .map_err(|_| InferenceError::GuardrailUnavailable)?
                    }
                    Err(limits::NoSlot) => {
                        let mut decision = policy.unavailable(workload);
                        // Busy, like an admission `Busy`: nothing was scanned,
                        // so the refusal writes no row and does not count
                        // toward the key's refusal bound (a saturated replica
                        // never throttles clean traffic).
                        if let Some(refusal) = decision
                            .refusal
                            .filter(|r| r.kind == RefusalKind::Unavailable)
                        {
                            count(&decision);
                            return Err(InferenceError::Guardrail(refusal));
                        }
                        // Refused anyway by a rule this release cannot run
                        // (fail-closed), or let through by fail-open ones:
                        // only their decisions are recorded, never one for a
                        // rule that was not scanned.
                        decision.events.retain(|e| !policy.runnable(e.rule_id));
                        decision
                    }
                }
            }
            input => timed(|| policy.decide(workload, input.as_ref())),
        };
        let (events, omitted) = rows(&decision);
        // Taken before the insert, so a request dropped during it counts too.
        let unadmitted = Unadmitted::new((!events.is_empty()).then_some(principal.key_id));
        if !events.is_empty() {
            self.repository
                .record_guardrails(&GuardrailRecord {
                    root_request_id: request_id,
                    principal: *principal,
                    operation,
                    model,
                    streamed,
                    events: &events,
                    omitted,
                })
                .await?;
        }
        verdict(principal, &decision)?;
        Ok(unadmitted)
    }
}

/// A request whose guardrail decisions wrote rows and that was not admitted
/// yet. Dropped before admission (refused by guardrails or by admission,
/// failed before an attempt, or abandoned), it counts toward its key's bound
/// of recorded requests (`RECORDED`): the guardrail step runs before
/// admission, so without it a key whose requests admission refuses could
/// make the replica write rows without bound. Once an attempt is admitted,
/// admission's own limits bound the request and it never counts, whatever
/// happens upstream.
#[must_use]
pub(super) struct Unadmitted {
    key: Option<Uuid>,
    admitted: std::sync::atomic::AtomicBool,
}
impl Unadmitted {
    fn new(key: Option<Uuid>) -> Self {
        Self {
            key,
            admitted: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// An attempt of the request was admitted: it does not count.
    pub(super) fn admitted(&self) {
        self.admitted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Drop for Unadmitted {
    fn drop(&mut self) {
        if let Some(key) = self.key
            && !self.admitted.load(std::sync::atomic::Ordering::Relaxed)
        {
            RECORDED.note(key, limits::now());
        }
    }
}

/// Run a decision, timing only the scan (never a slot wait).
fn timed(decide: impl FnOnce() -> Decision) -> Decision {
    let started = std::time::Instant::now();
    let decision = decide();
    crate::metrics::METRICS.guardrail_detector_seconds("pattern", started.elapsed());
    decision
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
