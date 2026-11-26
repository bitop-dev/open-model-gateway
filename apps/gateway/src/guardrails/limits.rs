//! Per-replica bounds on guardrail work, so one workspace or one key cannot
//! take the replica's inspection capacity from others (docs/guardrails-design.md
//! §4, "Capacity").
//!
//! - **Scan slots.** A scan that leaves the async workers (an input over
//!   64 KiB, or any input a custom pattern scans) takes a slot of one of two
//!   pools: scans of policies with local-layer (user-written) custom patterns
//!   use their own pool, so they never delay requests whose policy has none.
//!   A workspace holds at most `SHARE` slots of a pool at once and has at most
//!   `WAITERS` requests in the step (running or waiting); more are refused at
//!   once. The wait for a slot is bounded (`WAIT`), never the request
//!   deadline. Both end in a retryable `guardrail_unavailable`.
//! - **Refusals.** A key whose requests guardrails refused `REFUSALS_PER_MINUTE`
//!   times in the current minute on this replica gets 429
//!   `guardrail_rate_limited` before any scan, until the minute ends (a
//!   refused request is never admitted, so no other limit counts it).
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// Slots of one pool per replica.
pub const SLOTS: usize = 8;
/// Slots of one pool a workspace may hold at once.
pub const SHARE: usize = 2;
/// Requests of one workspace in the guardrail step at once (running or
/// waiting) per pool.
pub const WAITERS: usize = 4;
/// The longest wait for a slot.
pub const WAIT: Duration = Duration::from_secs(1);

/// A pool of scan slots with per-workspace shares.
pub struct ScanSlots {
    pool: Arc<Semaphore>,
    share: usize,
    waiters: usize,
    wait: Duration,
    workspaces: Mutex<HashMap<Uuid, (usize, Arc<Semaphore>)>>,
}

/// No slot now: the scan cannot run (retryable).
#[derive(Debug, PartialEq, Eq)]
pub struct NoSlot;

/// A held slot: released when dropped (move it into the scan, so a dropped
/// request keeps it until the scan ends).
pub struct ScanSlot {
    _pool: OwnedSemaphorePermit,
    _share: OwnedSemaphorePermit,
    _entered: Entered,
}

/// One request of a workspace in the step; leaves on drop.
struct Entered {
    slots: Arc<ScanSlots>,
    workspace: Uuid,
}
impl Drop for Entered {
    fn drop(&mut self) {
        let mut map = self
            .slots
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&self.workspace) {
            entry.0 -= 1;
            if entry.0 == 0 {
                map.remove(&self.workspace);
            }
        }
    }
}

impl ScanSlots {
    pub fn new(slots: usize, share: usize, waiters: usize, wait: Duration) -> Arc<Self> {
        Arc::new(Self {
            pool: Arc::new(Semaphore::new(slots)),
            share,
            waiters,
            wait,
            workspaces: Mutex::new(HashMap::new()),
        })
    }
    /// A slot for a scan of `workspace`: refused at once when the workspace
    /// already has its bound of requests in the step, and after `wait` when
    /// no slot came free.
    pub async fn acquire(self: &Arc<Self>, workspace: Uuid) -> Result<ScanSlot, NoSlot> {
        let share = {
            let mut map = self
                .workspaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = map
                .entry(workspace)
                .or_insert_with(|| (0, Arc::new(Semaphore::new(self.share))));
            if entry.0 >= self.waiters {
                return Err(NoSlot);
            }
            entry.0 += 1;
            Arc::clone(&entry.1)
        };
        let entered = Entered {
            slots: Arc::clone(self),
            workspace,
        };
        let deadline = tokio::time::Instant::now() + self.wait;
        let share = tokio::time::timeout_at(deadline, share.acquire_owned())
            .await
            .map_err(|_| NoSlot)?
            .map_err(|_| NoSlot)?;
        let pool = tokio::time::timeout_at(deadline, Arc::clone(&self.pool).acquire_owned())
            .await
            .map_err(|_| NoSlot)?
            .map_err(|_| NoSlot)?;
        Ok(ScanSlot {
            _pool: pool,
            _share: share,
            _entered: entered,
        })
    }
    /// Slots free in the pool (tests).
    pub fn available(&self) -> usize {
        self.pool.available_permits()
    }
}

/// Scans of policies with local-layer custom patterns.
pub static LOCAL_SCANS: LazyLock<Arc<ScanSlots>> =
    LazyLock::new(|| ScanSlots::new(SLOTS, SHARE, WAITERS, WAIT));
/// Every other scan that leaves the async workers.
pub static SHARED_SCANS: LazyLock<Arc<ScanSlots>> =
    LazyLock::new(|| ScanSlots::new(SLOTS, SHARE, WAITERS, WAIT));

/// Guardrail refusals a key may have in one minute on one replica before it
/// is throttled.
pub const REFUSALS_PER_MINUTE: u32 = 30;
/// Keys tracked at once; a key beyond them is not throttled (memory stays
/// bounded; the tracked set restarts every minute).
pub const TRACKED_KEYS: usize = 65_536;

/// Counts guardrail refusals per key in the current minute.
pub struct RefusalLimiter {
    limit: u32,
    capacity: usize,
    minute: Mutex<(u64, HashMap<Uuid, u32>)>,
}
impl RefusalLimiter {
    pub fn new(limit: u32, capacity: usize) -> Self {
        Self {
            limit,
            capacity,
            minute: Mutex::new((0, HashMap::new())),
        }
    }
    fn at<T>(&self, now: u64, f: impl FnOnce(&mut HashMap<Uuid, u32>) -> T) -> T {
        let mut guard = self
            .minute
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.0 != now / 60 {
            *guard = (now / 60, HashMap::new());
        }
        f(&mut guard.1)
    }
    /// `Err(seconds)` until the minute ends when `key` reached the limit.
    pub fn check(&self, key: Uuid, now: u64) -> Result<(), u32> {
        let limit = self.limit;
        self.at(now, |map| match map.get(&key) {
            Some(n) if *n >= limit => Err(u32::try_from(60 - now % 60).unwrap_or(60)),
            _ => Ok(()),
        })
    }
    /// Count one refusal of `key`.
    pub fn note(&self, key: Uuid, now: u64) {
        let capacity = self.capacity;
        self.at(now, |map| {
            if map.len() < capacity || map.contains_key(&key) {
                *map.entry(key).or_insert(0) += 1;
            }
        });
    }
}

/// This replica's guardrail refusals.
pub static REFUSALS: LazyLock<RefusalLimiter> =
    LazyLock::new(|| RefusalLimiter::new(REFUSALS_PER_MINUTE, TRACKED_KEYS));

/// Seconds since the epoch (the refusal limiter's clock).
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Decision rows written per request: the refused one first, then the
/// others; the rest are counted on the first row (`omitted`).
pub const MAX_EVENT_ROWS: usize = 16;

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
