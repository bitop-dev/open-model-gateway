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
//!     generation's, so a count never takes a generation permit.
//!
//!   Each refusal is the Anthropic-shaped 429 (`Busy`), before any upstream
//!   call.
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
/// The count limits' state in one process: per-key and per-connection
/// windows, the count-only concurrency pool and the window clock.
pub(crate) struct Counts {
    keys: Limiter<Uuid>,
    connections: Limiter<String>,
    pool: Arc<Semaphore>,
    minute: fn() -> i64,
}
impl Counts {
    pub(crate) fn new(concurrency: usize) -> Self {
        Self {
            keys: Limiter::default(),
            connections: Limiter::default(),
            pool: Arc::new(Semaphore::new(concurrency)),
            minute: current_minute,
        }
    }
    /// The same limits with a fixed window clock, for deterministic tests.
    #[cfg(all(test, feature = "integration-tests"))]
    pub(crate) fn with_clock(concurrency: usize, minute: fn() -> i64) -> Self {
        Self {
            minute,
            ..Self::new(concurrency)
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
        self.counts = Arc::new(Counts::with_clock(self.limits.count_concurrency, minute));
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
        crate::metrics::METRICS.token_count(match &result {
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
        });
        result.map_err(|e| match e {
            Limited => InferenceError::Busy,
            Failed(e) => e,
        })
    }

    async fn count(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<TokenCount, CountError> {
        let minute = (self.counts.minute)();
        if !self.counts.keys.admit(
            principal.key_id,
            minute,
            self.limits.counts_per_key_per_minute,
        ) {
            return Err(Limited);
        }
        // Never generation's `capacity`: a slow count cannot pin a permit
        // that inference needs.
        let _permit = self
            .counts
            .pool
            .clone()
            .try_acquire_owned()
            .map_err(|_| Limited)?;
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
impl From<InferenceError> for CountError {
    fn from(error: InferenceError) -> Self {
        Failed(error)
    }
}

#[cfg(test)]
#[path = "count_tests.rs"]
mod tests;
