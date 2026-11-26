//! `POST /v1/messages/count_tokens` (#72): the input-token count of a Messages
//! request, authorized like inference but never generation.
//!
//! - **Authority.** The workspace and key come from the validated inference
//!   key; the model must be one the key may use now (`live_candidates` reads
//!   the key, the workspace grant, the catalog and the key restriction live)
//!   and must serve the Messages protocol. The route is the first one
//!   inference routing would choose (`route_plan`, read-only); a count never
//!   fails over.
//! - **Source.** An adapter with a native counter answers (Anthropic asks
//!   Anthropic, with the connection's own credential). Every other route gets
//!   the gateway estimate below. The response says which (`TokenCountSource`).
//! - **Accounting.** No execution, reservation, hold or ledger entry is
//!   written, nothing is billed, and the result is never an admission bound.
//!   Routing health is not observed: a count is not an attempt.
//! - **Rate.** A count does not consume the key's request-rate allowance and
//!   consults no budget or rate limit: that allowance is the per-minute
//!   counters that reservation and execution rows maintain, and a count
//!   writes neither. A key that cannot run inference can still count. Counts
//!   are metered by their own per-replica bounds instead, because on
//!   Anthropic routes they reach the provider with the installation's
//!   credential (`EngineLimits`, configured at startup):
//!   - `counts_per_key_per_minute` per key per UTC minute;
//!   - `counts_per_connection_per_minute` provider-backed counts per provider
//!     connection credential per UTC minute (estimates call no provider);
//!   - `count_concurrency` counts at once, from a pool separate from
//!     generation's, so a count never takes a generation permit;
//!   - [`COUNTS_IN_FLIGHT_PER_KEY`] counts at once per key, so one key's slow
//!     counts cannot hold the whole pool.
//!
//!   Each refusal is the Anthropic-shaped 429 (`Busy`), before any upstream
//!   call. The key's window is spent only once the count holds its pool
//!   permit and in-flight slot, so a count refused for concurrency does not
//!   use up the key's minute.
//! - **Cancellation.** The count runs inside the request future; a client
//!   disconnect drops it, which cancels the upstream request. There is
//!   nothing to settle.
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::Semaphore;

use tokio::time::{Instant, timeout_at};
use uuid::Uuid;

use super::{
    Engine,
    error::InferenceError,
    types::{ApiProtocol, ChatRequest},
};
use crate::auth::Principal;

/// Default counts one gateway process admits per key per UTC minute
/// (`GATEWAY_COUNT_TOKENS_PER_KEY_PER_MINUTE`).
pub const COUNTS_PER_KEY_PER_MINUTE: u32 = 60;
/// Default provider-backed counts one gateway process admits per provider
/// connection credential per UTC minute
/// (`GATEWAY_COUNT_TOKENS_PER_CONNECTION_PER_MINUTE`).
pub const COUNTS_PER_CONNECTION_PER_MINUTE: u32 = 600;
/// Default counts one gateway process runs at once
/// (`GATEWAY_COUNT_TOKENS_CONCURRENCY`).
pub const COUNT_CONCURRENCY: usize = 8;
/// Counts one key may have running at once in one gateway process. Smaller
/// than the default pool, so other keys keep counting while one key's
/// counts wait on a slow provider; a pool configured this small or smaller
/// is itself the tighter bound.
pub const COUNTS_IN_FLIGHT_PER_KEY: usize = 2;
/// Keys tracked at once; older minutes are dropped first, and a full table
/// of current-minute keys refuses new keys rather than growing.
const TRACKED_KEYS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenCountSource {
    /// The provider counted the request.
    Provider,
    /// The gateway estimated it ([`estimate`]).
    Estimate,
}
impl TokenCountSource {
    /// The `x-omg-token-count` response header value.
    pub fn header(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Estimate => "estimate; method=utf8-bytes-div-4",
        }
    }
}

#[derive(Clone, Copy)]
pub struct TokenCount {
    pub input_tokens: u64,
    pub source: TokenCountSource,
}

/// The gateway estimate: the UTF-8 bytes of what the request would send as
/// input (system and message text, tool-call names and arguments, tool
/// results, and each tool's name, description and JSON schema), divided by
/// four and rounded up. It approximates common tokenizers on English text
/// and is not any model's tokenizer: it can differ from the provider's count
/// in either direction (code and non-Latin scripts usually tokenize denser).
/// Thinking history is not counted, because translated routes remove it.
pub fn estimate(request: &ChatRequest) -> u64 {
    let messages = request.messages.iter().map(|m| {
        m.content.as_ref().map_or(0, String::len)
            + m.tool_call_id.as_ref().map_or(0, String::len)
            + m.tool_calls
                .iter()
                .map(|c| c.id.len() + c.name.len() + c.arguments.len())
                .sum::<usize>()
    });
    let tools = request.tools.iter().map(|t| {
        t.name.len()
            + t.description.as_ref().map_or(0, String::len)
            + serde_json::to_vec(&t.parameters).map_or(0, |v| v.len())
    });
    let bytes = messages
        .chain(tools)
        .fold(0_u64, |sum, n| sum.saturating_add(n as u64));
    bytes.div_ceil(4)
}

/// Fixed-window admission of counts per key (an API key, or a provider
/// connection credential) in this process.
pub(crate) struct Limiter<K> {
    windows: Mutex<HashMap<K, (i64, u32)>>,
}
impl<K> Default for Limiter<K> {
    fn default() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }
}
impl<K: Hash + Eq> Limiter<K> {
    pub(crate) fn admit(&self, key: K, minute: i64, limit: u32) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if windows.len() >= TRACKED_KEYS && !windows.contains_key(&key) {
            windows.retain(|_, (m, _)| *m == minute);
            if windows.len() >= TRACKED_KEYS {
                return false;
            }
        }
        let window = windows.entry(key).or_insert((minute, 0));
        if window.0 != minute {
            *window = (minute, 0);
        }
        if window.1 >= limit {
            return false;
        }
        window.1 += 1;
        true
    }
}
/// Counts running per key in this process. A key has an entry only while it
/// holds a slot, and a slot is taken only with a pool permit, so the table
/// never holds more keys than the pool has permits.
#[derive(Default)]
struct InFlight(Mutex<HashMap<Uuid, usize>>);
impl InFlight {
    fn enter(&self, key: Uuid, limit: usize) -> Option<Slot<'_>> {
        let mut keys = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let running = keys.entry(key).or_insert(0);
        if *running >= limit {
            return None;
        }
        *running += 1;
        Some(Slot { table: self, key })
    }
}
/// One running count of one key. Dropping it (on every return, and when the
/// request future is dropped by a disconnect) frees the slot.
struct Slot<'a> {
    table: &'a InFlight,
    key: Uuid,
}
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut keys = self.table.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = keys.get_mut(&self.key) {
            *running -= 1;
            if *running == 0 {
                keys.remove(&self.key);
            }
        }
    }
}

/// The count limits' state in one process: per-key and per-connection
/// windows, the count-only concurrency pool, per-key in-flight slots and the
/// window clock.
pub(crate) struct Counts {
    keys: Limiter<Uuid>,
    connections: Limiter<String>,
    pool: Arc<Semaphore>,
    in_flight: InFlight,
    minute: fn() -> i64,
    /// A private registry for a test that asserts exact outcome counts.
    #[cfg(all(test, feature = "integration-tests"))]
    metrics: Option<&'static crate::metrics::Metrics>,
}
impl Counts {
    pub(crate) fn new(concurrency: usize) -> Self {
        Self {
            keys: Limiter::default(),
            connections: Limiter::default(),
            pool: Arc::new(Semaphore::new(concurrency)),
            in_flight: InFlight::default(),
            minute: current_minute,
            #[cfg(all(test, feature = "integration-tests"))]
            metrics: None,
        }
    }
}
/// The upstream credential a provider-backed count spends: the connection's
/// provider, credential reference and endpoint (two connections naming the
/// same credential share its window).
fn connection(deployment: &super::types::Deployment) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        deployment.provider,
        deployment.credential_ref,
        deployment.endpoint.as_deref().unwrap_or("")
    )
}
fn current_minute() -> i64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (seconds / 60) as i64
}

impl Engine {
    /// The same engine whose count windows use `minute` as the clock, so a
    /// test controls the window instead of racing the UTC minute.
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) fn with_count_clock(mut self, minute: fn() -> i64) -> Self {
        Arc::get_mut(&mut self.counts)
            .expect("set before the engine is shared")
            .minute = minute;
        self
    }

    /// The same engine recording count outcomes in `metrics` instead of the
    /// process registry, so a test's counts are exact.
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) fn with_count_metrics(mut self, metrics: &'static crate::metrics::Metrics) -> Self {
        Arc::get_mut(&mut self.counts)
            .expect("set before the engine is shared")
            .metrics = Some(metrics);
        self
    }

    /// Counts the input tokens of a validated Messages request (`max_tokens`
    /// and `stream` absent). See the module documentation for the rules.
    /// Every outcome is counted in `gateway_token_counts_total{outcome}`
    /// (no key, workspace or model label).
    pub async fn count_tokens(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<TokenCount, InferenceError> {
        let result = self.count(principal, request, request_id).await;
        let outcome = match &result {
            Ok(TokenCount {
                source: TokenCountSource::Provider,
                ..
            }) => "provider",
            Ok(TokenCount {
                source: TokenCountSource::Estimate,
                ..
            }) => "estimate",
            Err(Limited) => "limited",
            Err(_) => "error",
        };
        #[cfg(all(test, feature = "integration-tests"))]
        if let Some(metrics) = self.counts.metrics {
            metrics.token_count(outcome);
            return result.map_err(CountError::into_inference);
        }
        crate::metrics::METRICS.token_count(outcome);
        result.map_err(CountError::into_inference)
    }

    async fn count(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<TokenCount, CountError> {
        // Never generation's `capacity`: a slow count cannot pin a permit
        // that inference needs.
        let _permit = self
            .counts
            .pool
            .clone()
            .try_acquire_owned()
            .map_err(|_| Limited)?;
        let _slot = self
            .counts
            .in_flight
            .enter(principal.key_id, COUNTS_IN_FLIGHT_PER_KEY)
            .ok_or(Limited)?;
        // Spent only now, so a count refused for concurrency (a client
        // retrying through a full pool) keeps the key's minute.
        let minute = (self.counts.minute)();
        if !self.counts.keys.admit(
            principal.key_id,
            minute,
            self.limits.counts_per_key_per_minute,
        ) {
            return Err(Limited);
        }
        let deadline = Instant::now() + self.limits.request_timeout;
        let deployments = timeout_at(
            deadline,
            self.repository.live_candidates(&principal, &request.model),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable.into());
        }
        let (candidates, refused) =
            self.eligible_candidates(deployments, &request, ApiProtocol::Messages);
        if candidates.is_empty() {
            if refused.is_empty() {
                return Err(InferenceError::Unsupported.into());
            }
            return Err(self
                .refuse(&principal, &request, &refused, request_id, deadline)
                .await
                .err()
                .unwrap_or(InferenceError::Unsupported)
                .into());
        }
        // A count may send the request's text to the provider (a provider
        // count; a local estimate sends nothing): the guardrails of a Messages
        // request apply before any route is used.
        let facts = super::guardrails::Guarded {
            request_id,
            operation: "count_tokens",
            model: &request.model,
            streamed: false,
            workload: crate::guardrails::policy::Workload::Generation,
        };
        let inspect = || Some(crate::guardrails::enforce::chat_inspection(&request));
        // Counts are already bounded per key and minute (above), so a count
        // never counts as unanswered.
        timeout_at(deadline, self.guard_input(&principal, facts, inspect))
            .await
            .map_err(|_| InferenceError::Timeout)??
            .answered();
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan(&principal, &request.model, &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        let deployment = plan
            .deployment_ids
            .first()
            .and_then(|id| candidates.iter().find(|d| d.id == *id))
            .ok_or(InferenceError::Configuration)?;
        let adapter = self
            .registry
            .get(&deployment.provider)
            .ok_or(InferenceError::Configuration)?;
        if !adapter.counts_input_tokens() {
            return Ok(TokenCount {
                input_tokens: estimate(&request),
                source: TokenCountSource::Estimate,
            });
        }
        // Checked only now, so a refused or estimated count never spends the
        // connection's window.
        if !self.counts.connections.admit(
            connection(deployment),
            minute,
            self.limits.counts_per_connection_per_minute,
        ) {
            return Err(Limited);
        }
        let counted = timeout_at(deadline, adapter.count_input_tokens(deployment, &request))
            .await
            .map_err(|_| InferenceError::Timeout)??;
        Ok(match counted {
            Some(input_tokens) => TokenCount {
                input_tokens,
                source: TokenCountSource::Provider,
            },
            None => TokenCount {
                input_tokens: estimate(&request),
                source: TokenCountSource::Estimate,
            },
        })
    }
}

/// Why a count was not answered: one of its own limits, or any other error.
enum CountError {
    Limited,
    Failed(InferenceError),
}
use CountError::{Failed, Limited};
impl CountError {
    fn into_inference(self) -> InferenceError {
        match self {
            Limited => InferenceError::Busy,
            Failed(e) => e,
        }
    }
}
impl From<InferenceError> for CountError {
    fn from(error: InferenceError) -> Self {
        Failed(error)
    }
}

#[cfg(test)]
#[path = "count_tests.rs"]
mod tests;
