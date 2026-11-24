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
//! - **Rate.** A count does not consume the key's request-rate allowance:
//!   that allowance is the per-minute counters that reservation and
//!   execution rows maintain, and a count writes neither. Counts are still
//!   metered, because on Anthropic routes they reach the provider: each holds
//!   one of the engine's concurrency permits, and each gateway process admits
//!   at most [`COUNTS_PER_KEY_PER_MINUTE`] counts per key per UTC minute
//!   (`EngineLimits::counts_per_key_per_minute`; 429 beyond that).
//! - **Cancellation.** The count runs inside the request future; a client
//!   disconnect drops it, which cancels the upstream request. There is
//!   nothing to settle.
use std::{collections::HashMap, sync::Mutex};

use tokio::time::{Instant, timeout_at};
use uuid::Uuid;

use super::{
    Engine,
    error::InferenceError,
    types::{ApiProtocol, ChatRequest},
};
use crate::auth::Principal;

/// Counts one gateway process admits per key per UTC minute, unless a test
/// engine sets another limit.
pub const COUNTS_PER_KEY_PER_MINUTE: u32 = 60;
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

/// Fixed-window per-key admission of counts in this process.
#[derive(Default)]
pub(crate) struct Limiter {
    windows: Mutex<HashMap<Uuid, (i64, u32)>>,
}
impl Limiter {
    pub(crate) fn admit(&self, key: Uuid, minute: i64, limit: u32) -> bool {
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
fn current_minute() -> i64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (seconds / 60) as i64
}

impl Engine {
    /// Counts the input tokens of a validated Messages request (`max_tokens`
    /// and `stream` absent). See the module documentation for the rules.
    pub async fn count_tokens(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<TokenCount, InferenceError> {
        if !self.counts.admit(
            principal.key_id,
            current_minute(),
            self.limits.counts_per_key_per_minute,
        ) {
            return Err(InferenceError::Busy);
        }
        let _permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| crate::metrics::capacity_denied())?;
        let deadline = Instant::now() + self.limits.request_timeout;
        let deployments = timeout_at(
            deadline,
            self.repository.live_candidates(&principal, &request.model),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let (candidates, refused) =
            self.eligible_candidates(deployments, &request, ApiProtocol::Messages);
        if candidates.is_empty() {
            if refused.is_empty() {
                return Err(InferenceError::Unsupported);
            }
            return Err(self
                .refuse(&principal, &request, &refused, request_id, deadline)
                .await
                .err()
                .unwrap_or(InferenceError::Unsupported));
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

#[cfg(test)]
#[path = "count_tests.rs"]
mod tests;
