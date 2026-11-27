//! A line's worker permits while it waits to be inspected again, and how a
//! line its batch stopped meanwhile is reported.
use super::*;

/// How a line its batch stopped while it waited to be inspected again ends.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::jobs::runner) enum Stopped {
    /// No attempt of it was admitted (`LineRun::admitted`): reported like
    /// the lines the stop left unrun.
    NeverRan(NotRun),
    /// An earlier attempt (this execution) was admitted and failed with this
    /// error: the line is reported with it.
    AfterAttempt(Uuid, InferenceError),
}

/// A line's per-batch and process worker permits (the process permit given
/// back while the line waits to be inspected again), and how often it waited
/// for each cause.
pub(in crate::jobs::runner) struct Workers {
    /// Set when the line's batch stopped while it waited.
    pub(super) stopped: Option<Stopped>,
    /// The line's latest admitted attempt, and how it failed (a later
    /// attempt is the current one).
    pub(super) ran: Option<(Uuid, InferenceError)>,
    pub(super) busy: u32,
    pub(super) spent: u32,
    _batch: tokio::sync::OwnedSemaphorePermit,
    pub(super) process: Option<tokio::sync::OwnedSemaphorePermit>,
    pool: Arc<tokio::sync::Semaphore>,
}
impl Workers {
    pub(in crate::jobs::runner) fn new(
        (batch, process): (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
    ) -> Self {
        Self {
            stopped: None,
            ran: None,
            busy: 0,
            spent: 0,
            _batch: batch,
            pool: Arc::clone(process.semaphore()),
            process: Some(process),
        }
    }
    /// How a line its batch stopped (`stop`) while it waited is reported.
    pub(super) fn stopped_by(&self, stop: Stop) -> Stopped {
        match self.ran {
            Some((ran, error)) => Stopped::AfterAttempt(ran, error),
            None => Stopped::NeverRan(NotRun::from(stop)),
        }
    }
    /// Whether a spent budget would be tried again.
    pub(super) fn spent_left(&self) -> bool {
        self.spent < SPENT_TRIES
    }
    /// The process permit again; `false` if the pool closed.
    pub(super) async fn reacquire(&mut self) -> bool {
        match Arc::clone(&self.pool).acquire_owned().await {
            Ok(permit) => {
                self.process = Some(permit);
                true
            }
            Err(_) => false,
        }
    }
}

/// Tests: cancel a batch at a line's Nth guardrail wait (a cancel reaches a
/// running batch only through its lease renewal, too late for a test).
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) mod tests_stop {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use uuid::Uuid;
    static AT: LazyLock<Mutex<HashMap<Uuid, u32>>> = LazyLock::new(Default::default);
    /// Cancel batch `job` at its `nth` wait (1-based).
    pub(crate) fn cancel_at(job: Uuid, nth: u32) {
        AT.lock().unwrap().insert(job, nth);
    }
    pub(in crate::jobs::runner) fn now(job: Uuid) -> bool {
        let mut at = AT.lock().unwrap();
        match at.get_mut(&job) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                at.remove(&job);
                true
            }
            None => false,
        }
    }
}
